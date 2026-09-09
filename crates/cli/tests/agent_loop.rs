use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

type Server = (SocketAddr, thread::JoinHandle<Result<Vec<Value>>>);

fn read_request(stream: &mut TcpStream, method: &str) -> Result<Value> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = match method {
        "GET" => "/models",
        _ => "/chat/completions",
    };
    assert!(line.starts_with(&format!("{method} {path} ")));
    let mut length = 0;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line == "\r\n" {
            break;
        }
        assert!(!line.is_empty(), "Incomplete HTTP headers");
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>()?;
        }
    }
    if length == 0 {
        return Ok(Value::Null);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn server(responses: Vec<(&'static str, Value)>) -> Result<Server> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for (method, response) in responses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Timed out waiting for request");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            requests.push(read_request(&mut stream, method)?);
            let body = response.to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
        }
        Ok(requests)
    });
    Ok((address, server))
}

fn requests(server: thread::JoinHandle<Result<Vec<Value>>>) -> Result<Value> {
    Ok(Value::Array(server.join().map_err(|_| {
        anyhow::anyhow!("Mock server thread failed")
    })??))
}

fn wait(mut command: Command) -> Result<Output> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            bail!("Agent did not exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(child.wait_with_output()?)
}

fn cli(address: SocketAddr) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ri"));
    command
        .env("MODEL", "mock-model")
        .env("HOME", "/nonexistent-ri-test-home")
        .env("OPENROUTER_API_KEY", "mock-key")
        .env("OPENROUTER_BASE_URL", format!("http://{address}"))
        .env("NO_PROXY", "127.0.0.1");
    command
}

#[test]
fn expands_project_skills_before_sending_the_prompt() -> Result<()> {
    let (address, handle) = server(vec![(
        "POST",
        json!({"choices":[{"message":{"content":"Reviewed"}}]}),
    )])?;
    let root = std::env::temp_dir().join(format!(
        "ri-skills-{}-{}",
        std::process::id(),
        address.port()
    ));
    let skill_dir = root.join(".ri/skills/review");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Review code\n---\nLook for regressions before style issues.",
    )?;
    let mut command = cli(address);
    command
        .current_dir(&root)
        .args(["--no-save", "-p", "$review the changes"]);
    let output = wait(command)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = requests(handle)?;
    let prompt = field(&requests, "/0/messages/0/content")?
        .as_str()
        .context("Missing prompt")?;
    assert!(prompt.starts_with("$review the changes"));
    assert!(prompt.contains("Look for regressions before style issues."));
    assert!(prompt.contains(&skill_dir.canonicalize()?.display().to_string()));
    let mut command = cli(address);
    command
        .current_dir(&root)
        .args(["--no-save", "-p", "$missing"]);
    let output = wait(command)?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Unknown skill '$missing'"));
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn run(responses: Vec<Value>, max_turns: usize) -> Result<(Output, Value)> {
    let (address, handle) = server(
        responses
            .into_iter()
            .map(|response| ("POST", response))
            .collect(),
    )?;
    let mut command = cli(address);
    command.args([
        "-p",
        "Read a file",
        "--no-save",
        "--max-turns",
        &max_turns.to_string(),
    ]);
    Ok((wait(command)?, requests(handle)?))
}

fn field<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value> {
    value
        .pointer(pointer)
        .with_context(|| format!("Missing JSON field {pointer}"))
}

fn tool(arguments: &Value) -> Value {
    json!({"choices": [{"message": {"tool_calls": [{
        "id": "read_1", "type": "function",
        "function": {"name": "Read", "arguments": arguments.to_string()}
    }]}}]})
}

#[test]
fn completes_a_tool_round_trip_and_recovers_from_errors() -> Result<()> {
    for arguments in [
        json!({"file_path": concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")}),
        json!({}),
    ] {
        let invalid = arguments.get("file_path").is_none();
        let (output, requests) = run(
            vec![
                tool(&arguments),
                json!({"choices": [{"message": {"content": "Done"}}]}),
            ],
            3,
        )?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"Done\n");
        let progress = String::from_utf8_lossy(&output.stderr);
        assert!(progress.contains("Requesting model response (1/3)"));
        assert!(progress.contains("Running Read (read_1)"));
        assert!(
            progress.contains("Read (read_1) completed")
                || progress.contains("Read (read_1) failed")
        );
        assert_eq!(requests.as_array().map(Vec::len), Some(2));
        assert_eq!(field(&requests, "/0/model")?, "mock-model");
        assert!(requests.pointer("/0/reasoning").is_none());
        assert_eq!(field(&requests, "/1/messages/2/role")?, "tool");
        assert_eq!(field(&requests, "/1/messages/2/tool_call_id")?, "read_1");
        let content = field(&requests, "/1/messages/2/content")?
            .as_str()
            .context("Missing content")?;
        if invalid {
            assert!(content.contains("Invalid Read arguments"));
            continue;
        }
        assert_eq!(content, include_str!("../../../Cargo.toml"));
    }
    Ok(())
}

fn config() -> Result<ri_agent::agent::AgentConfig> {
    use ri_agent::{
        agent::AgentConfig,
        config::{LuaConfig, ReasoningEffort},
        limits::Limits,
    };
    Ok(AgentConfig {
        model: "mock-model".into(),
        reasoning_effort: Some(ReasoningEffort::High),
        max_turns: std::num::NonZeroUsize::MIN.saturating_add(2),
        limits: Limits::default(),
        lua: LuaConfig::from_source(
            r"return {
            hooks = { before_prompt = function(text) return 'task: ' .. text end,
                after_response = function(text) return text .. '!' end },
            tools = {{ name = 'Echo', description = 'Echo', parameters = {type = 'object'},
                execute = function(args) return args.text end }},
        }",
            "test",
        )?,
    })
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn sqlite_resume_preserves_reasoning_tool_history_and_recovers_after_failed_turns()
-> Result<()> {
    use ri_agent::{
        agent::Session,
        config::ReasoningEffort,
        events::{Event, Output},
        sessions::SessionStore,
    };
    let (address, handle) = server(vec![
        (
            "POST",
            json!({"choices":[{"message":{"reasoning":"thinking", "reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","signature":"keep-exactly"}], "tool_calls":[{
                "id":"echo_1", "type":"function", "function":{"name":"Echo", "arguments":"{\"text\":\"hello\"}"}
            }]}}]}),
        ),
        ("POST", json!({"choices":[{"message":{"content":"Done"}}]})),
        ("POST", json!({"choices":[]})),
        (
            "POST",
            json!({"choices":[{"message":{"content":"Follow up"}}]}),
        ),
        (
            "POST",
            json!({"choices":[{"message":{"content":"New chat"}}]}),
        ),
    ])?;
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut session = Session::new(
        std::sync::Arc::new(ri_agent::openrouter::OpenRouter::new(
            &format!("http://{address}"),
            ri_agent::credentials::api_key("mock-key")?,
        )?),
        config()?,
        Output::channel(sender.clone()),
    );
    let directory = std::env::temp_dir().join(format!("ri-resume-{}", session.id()));
    let store = SessionStore::open(
        directory.join("sessions.sqlite3"),
        &std::env::current_dir()?,
    )?;
    session = session.with_store(store.clone());
    let id = session.id().to_owned();
    session.submit("first".into()).await?;
    drop(session);
    let mut session = Session::new(
        std::sync::Arc::new(ri_agent::openrouter::OpenRouter::new(
            &format!("http://{address}"),
            ri_agent::credentials::api_key("mock-key")?,
        )?),
        config()?,
        Output::channel(sender),
    )
    .with_store(store.clone());
    assert!(!session.resume(&id).await?);
    assert_eq!(
        session.selection().reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(session.history().len(), 3);
    assert!(session.submit("failed".into()).await.is_err());
    session.submit("second".into()).await?;
    session.clear();
    session.submit("fresh".into()).await?;
    let requests = requests(handle)?;
    assert_eq!(
        field(&requests, "/0/tools")?.as_array().map(Vec::len),
        Some(6)
    );
    assert_eq!(field(&requests, "/0/messages/0/content")?, "task: first");
    assert_eq!(field(&requests, "/0/reasoning/effort")?, "high");
    assert_eq!(field(&requests, "/1/messages/1/reasoning")?, "thinking");
    assert_eq!(
        field(&requests, "/1/messages/1/reasoning_details/0/signature")?,
        "keep-exactly"
    );
    assert_eq!(
        field(&requests, "/2/messages/1/reasoning_details")?,
        field(&requests, "/1/messages/1/reasoning_details")?
    );
    assert_eq!(field(&requests, "/1/messages/2/content")?, "hello");
    assert_eq!(
        field(&requests, "/3/messages")?.as_array().map(Vec::len),
        Some(5)
    );
    assert_eq!(field(&requests, "/3/messages/3/content")?, "Done!");
    assert_eq!(field(&requests, "/3/messages/4/content")?, "task: second");
    assert_eq!(
        field(&requests, "/4/messages")?.as_array().map(Vec::len),
        Some(1)
    );
    let mut assistant = Vec::new();
    let mut tools = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            Event::Assistant(text) => assistant.push(text),
            Event::Tool(text) => tools.push(text),
            _ => {}
        }
    }
    assert_eq!(assistant, ["Done!", "Follow up!", "New chat!"]);
    assert!(tools.first().is_some_and(|text| text.contains("hello")));
    assert_eq!(store.list().await?.len(), 2);
    std::fs::remove_dir_all(directory)?;
    Ok(())
}

#[test]
fn stops_at_the_turn_limit() -> Result<()> {
    let (output, requests) = run(vec![tool(&json!({}))], 1)?;
    assert!(!output.status.success());
    assert_eq!(requests.as_array().map(Vec::len), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Maximum model turns (1) reached"));
    Ok(())
}

#[test]
fn uses_dot_ri_for_configuration_and_sqlite_and_validates_reasoning() -> Result<()> {
    let (address, handle) = server(vec![
        (
            "GET",
            json!({"data":[{"id":"test/model", "supported_parameters":["tools"], "reasoning":{"supported_efforts":["high"]}}]}),
        ),
        ("POST", json!({"choices":[{"message":{"content":"Done"}}]})),
    ])?;
    let home =
        std::env::temp_dir().join(format!("ri-home-{}-{}", std::process::id(), address.port()));
    std::fs::create_dir_all(home.join(".ri"))?;
    std::fs::write(
        home.join(".ri/config.lua"),
        "return {settings={model='test/model',reasoning_effort='high'}}",
    )?;
    let mut command = cli(address);
    command
        .env("HOME", &home)
        .env_remove("MODEL")
        .args(["-p", "hello"]);
    let output = wait(command)?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.join(".ri/sessions.sqlite3").is_file());
    let requests = requests(handle)?;
    assert_eq!(field(&requests, "/1/model")?, "test/model");
    assert_eq!(field(&requests, "/1/reasoning/effort")?, "high");
    let mut command = cli(address);
    command
        .env("HOME", &home)
        .env_remove("OPENROUTER_API_KEY")
        .arg("--sessions");
    let output = wait(command)?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    std::fs::remove_dir_all(home)?;
    Ok(())
}
