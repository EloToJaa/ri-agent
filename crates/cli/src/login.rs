use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use ri_agent::{
    credentials,
    openrouter::{self, OpenRouter},
};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
};

pub async fn run(api_key_mode: bool) -> Result<()> {
    let key = if api_key_mode {
        let value = rpassword::prompt_password("OpenRouter API key: ")?;
        credentials::api_key(value)?
    } else {
        browser_login().await?
    };
    let path = credentials::default_path()?;
    credentials::save(&path, &key)?;
    println!("OpenRouter login saved to {}", path.display());
    Ok(())
}

async fn browser_login() -> Result<credentials::ApiKey> {
    let mut verifier_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("Binding OpenRouter login callback")?;
    listener
        .set_nonblocking(true)
        .context("Configuring OpenRouter login callback")?;
    let port = listener.local_addr()?.port();
    let callback = format!("http://localhost:{port}/callback");
    let url = format!(
        "{}?callback_url={}&code_challenge={}&code_challenge_method=S256&key_label=ri",
        openrouter::AUTH_URL,
        urlencoding::encode(&callback),
        challenge
    );
    println!("Opening OpenRouter login in your browser…");
    if !open_browser(&url) {
        println!("Open this URL manually:\n{url}");
    }
    let callback_listener = listener
        .try_clone()
        .context("Cloning OpenRouter login callback")?;
    let (mut stream, _) = tokio::task::spawn_blocking(move || accept_callback(&callback_listener))
        .await
        .context("Login callback task failed")??;
    let code = read_code(&mut stream)?;
    let client = OpenRouter::new(
        openrouter::DEFAULT_BASE_URL,
        credentials::api_key("login-exchange-placeholder")?,
    )?;
    client.exchange_code(&code, &verifier).await
}

fn accept_callback(listener: &TcpListener) -> Result<(TcpStream, std::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(10);
    loop {
        match listener.accept() {
            Ok(connection) => return Ok(connection),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                bail!("Timed out waiting for OpenRouter login callback; run 'ri login' again")
            }
            Err(error) => return Err(error).context("Accepting OpenRouter login callback"),
        }
    }
}

fn open_browser(url: &str) -> bool {
    let command = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "xdg-open"
    };
    std::process::Command::new(command)
        .args(if cfg!(target_os = "windows") {
            vec!["/C", "start", "", url]
        } else {
            vec![url]
        })
        .spawn()
        .is_ok()
}

fn read_code(stream: &mut TcpStream) -> Result<String> {
    stream.set_read_timeout(Some(std::time::Duration::from_mins(2)))?;
    let mut reader = BufReader::new(&mut *stream);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let target = request
        .split_whitespace()
        .nth(1)
        .context("Invalid login callback request")?;
    let query = target
        .split_once('?')
        .map_or("", |(_, query)| query.split('#').next().unwrap_or(""));
    let mut code = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(urlencoding::decode(value)?.into_owned()),
            "error" => error = Some(urlencoding::decode(value)?.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        write!(
            stream,
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nOpenRouter login was rejected."
        )?;
        bail!("OpenRouter login failed: {error}");
    }
    let code = code.context("OpenRouter callback did not contain an authorization code")?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 48\r\nConnection: close\r\n\r\nLogin complete. You can close this browser tab."
    )?;
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_and_rejection_callbacks_without_leaking_codes() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let join = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            read_code(&mut stream)
        });
        let mut client = TcpStream::connect(address)?;
        write!(
            client,
            "GET /callback?code=one-time-secret HTTP/1.1\r\nHost: localhost\r\n\r\n"
        )?;
        let code = join
            .join()
            .map_err(|_| anyhow::anyhow!("callback thread failed"))??;
        assert_eq!(code, "one-time-secret");
        Ok(())
    }
}
