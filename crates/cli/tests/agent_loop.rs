use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

fn server(responses: Vec<Value>) -> (std::net::SocketAddr, thread::JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Timed out waiting for request");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("POST /chat/completions "));
            let mut length = None;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty(), "Incomplete HTTP headers");
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut body = vec![0; length.unwrap()];
            reader.read_exact(&mut body).unwrap();
            requests.push(serde_json::from_slice(&body).unwrap());
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    (address, server)
}

fn run(responses: Vec<Value>, max_turns: usize) -> (Output, Vec<Value>) {
    let (address, server) = server(responses);
    let mut child = Command::new(env!("CARGO_BIN_EXE_codecrafters-claude-code"))
        .args(["-p", "Read a file", "--max-turns", &max_turns.to_string()])
        .env("MODEL", "mock-model")
        .env("XDG_CONFIG_HOME", "/nonexistent-ri-agent-test-config")
        .env("OPENROUTER_API_KEY", "mock-key")
        .env("OPENROUTER_BASE_URL", format!("http://{address}"))
        .env("NO_PROXY", "127.0.0.1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Agent did not exit");
        }
        thread::sleep(Duration::from_millis(10));
    }
    (child.wait_with_output().unwrap(), server.join().unwrap())
}

fn tool(arguments: Value) -> Value {
    json!({"choices": [{"message": {"tool_calls": [{
        "id": "read_1", "type": "function",
        "function": {"name": "Read", "arguments": arguments.to_string()}
    }]}}]})
}

#[test]
fn completes_a_tool_round_trip_and_recovers_from_errors() {
    for arguments in [
        json!({"file_path": concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")}),
        json!({}),
    ] {
        let invalid = arguments.get("file_path").is_none();
        let (output, requests) = run(
            vec![
                tool(arguments),
                json!({"choices": [{"message": {"content": "Done"}}]}),
            ],
            3,
        );
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
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "mock-model");
        let result = &requests[1]["messages"][2];
        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], "read_1");
        let content = result["content"].as_str().unwrap();
        if invalid {
            assert!(content.contains("Invalid Read arguments"));
            continue;
        }
        assert_eq!(content, include_str!("../../../Cargo.toml"));
    }
}

#[tokio::test]
async fn session_retains_history_runs_lua_tools_and_recovers_after_failed_turns() {
    use ri_agent::{
        agent::{AgentConfig, Session},
        config::LuaConfig,
        events::{Event, Output},
        limits::Limits,
    };
    let lua = LuaConfig::from_source(
        r#"return {
        hooks = {
            before_prompt = function(text) return 'task: ' .. text end,
            after_response = function(text) return text .. '!' end,
        },
        tools = {{ name = 'Echo', description = 'Echo',
            parameters = {type = 'object'},
            execute = function(args) return args.text end }},
    }"#,
        "test",
    )
    .unwrap();
    let (address, server) = server(vec![
        json!({"choices":[{"message":{"tool_calls":[{
            "id":"echo_1", "type":"function", "function":{
                "name":"Echo", "arguments":"{\"text\":\"hello\"}"
            }
        }]}}]}),
        json!({"choices":[{"message":{"content":"Done"}}]}),
        json!({"choices":[]}),
        json!({"choices":[{"message":{"content":"Follow up"}}]}),
        json!({"choices":[{"message":{"content":"New chat"}}]}),
    ]);
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut session = Session::new(
        format!("http://{address}"),
        "mock-key".into(),
        AgentConfig {
            model: "mock-model".into(),
            max_turns: std::num::NonZeroUsize::new(3).unwrap(),
            limits: Limits::default(),
            lua,
        },
        Output::channel(sender),
    );
    session.submit("first".into()).await.unwrap();
    assert!(session.submit("failed".into()).await.is_err());
    session.submit("second".into()).await.unwrap();
    session.clear();
    session.submit("fresh".into()).await.unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests[0]["tools"].as_array().unwrap().len(), 4);
    assert_eq!(requests[0]["messages"][0]["content"], "task: first");
    assert_eq!(requests[1]["messages"][2]["content"], "hello");
    assert_eq!(requests[3]["messages"].as_array().unwrap().len(), 5);
    assert_eq!(requests[3]["messages"][3]["content"], "Done!");
    assert_eq!(requests[3]["messages"][4]["content"], "task: second");
    assert_eq!(requests[4]["messages"].as_array().unwrap().len(), 1);
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
    assert!(tools[0].contains("hello"));
}

#[test]
fn stops_at_the_turn_limit() {
    let (output, requests) = run(vec![tool(json!({}))], 1);
    assert!(!output.status.success());
    assert_eq!(requests.len(), 1);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Maximum model turns (1) reached"));
}
