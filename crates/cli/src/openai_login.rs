use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use ri_agent::{
    credentials::{self, CodexCredentials},
    providers::codex,
};
use secrecy::SecretString;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

pub async fn run(manual: bool) -> Result<()> {
    let mut verifier_bytes = [0_u8; 64];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut state_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut state_bytes);
    let state = URL_SAFE_NO_PAD.encode(state_bytes);
    let listener = callback_listener(manual)?;
    let redirect = REDIRECT_URI;
    let url = format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator=ri",
        codex::ISSUER,
        codex::CLIENT_ID,
        urlencoding::encode(redirect),
        urlencoding::encode(
            "openid profile email offline_access api.connectors.read api.connectors.invoke"
        ),
        challenge,
        state
    );
    let (code, mut callback_stream) = if let Some(listener) = listener {
        println!("Opening ChatGPT login in your browser…");
        if !std::process::Command::new("xdg-open")
            .arg(&url)
            .status()
            .is_ok_and(|status| status.success())
        {
            println!("Open this URL manually:\n{url}");
        }
        let (mut stream, _) = tokio::task::spawn_blocking(move || listener.accept()).await??;
        stream.set_read_timeout(Some(Duration::from_mins(2)))?;
        let mut data = [0_u8; 16384];
        let length = stream.read(&mut data)?;
        let request = std::str::from_utf8(data.get(..length).context("Invalid callback request")?)?;
        let target = request
            .split_whitespace()
            .nth(1)
            .context("Invalid callback request")?;
        let code = code_from_callback_url(&format!("http://localhost:1455{target}"), &state)?;
        (code, Some(stream))
    } else {
        println!("Open this URL in a browser:\n{url}");
        println!(
            "After approval, the localhost page may fail to load. Copy its full URL from the browser address bar."
        );
        let callback = rpassword::prompt_password("Paste the redirected URL (input hidden): ")
            .context("Reading OpenAI callback URL")?;
        (code_from_callback_url(callback.trim(), &state)?, None)
    };
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_mins(1))
        .build()?
        .post(format!("{}/oauth/token", codex::ISSUER))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect),
            ("client_id", codex::CLIENT_ID),
            ("code_verifier", &verifier),
        ])
        .send()
        .await
        .context("Exchanging OpenAI authorization code")?
        .error_for_status()
        .context("OpenAI token exchange failed")?;
    let tokens = response
        .json::<Tokens>()
        .await
        .context("Invalid OpenAI token response")?;
    let account_id = jwt_account_id(&tokens.id_token)?;
    let credential = CodexCredentials {
        access_token: SecretString::from(tokens.access_token),
        refresh_token: SecretString::from(tokens.refresh_token),
        id_token: SecretString::from(tokens.id_token),
        account_id,
    };
    let path = credentials::default_path()?;
    credentials::save_codex(&path, &credential)?;
    if let Some(stream) = &mut callback_stream {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<h1>Login complete</h1><p>You can return to ri.</p>")?;
    }
    println!("OpenAI Codex login saved to {}", path.display());
    Ok(())
}

fn callback_listener(manual: bool) -> Result<Option<TcpListener>> {
    if manual {
        return Ok(None);
    }
    TcpListener::bind(("127.0.0.1", 1455))
        .map(Some)
        .context("Binding OpenAI login callback on port 1455; use --manual if it is unavailable")
}

fn code_from_callback_url(input: &str, expected_state: &str) -> Result<String> {
    // Never include the pasted URL or upstream error text in errors: both can contain secrets.
    let url = reqwest::Url::parse(input).context("Invalid OpenAI callback URL")?;
    if url.scheme() != "http"
        || url.host_str() != Some("localhost")
        || url.port_or_known_default() != Some(1455)
        || url.path() != "/auth/callback"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("Expected the full http://localhost:1455/auth/callback URL from this login attempt");
    }
    let mut state = None;
    let mut code = None;
    let mut rejected = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" => {
                if state.replace(value.into_owned()).is_some() {
                    bail!("Duplicate OpenAI callback state");
                }
            }
            "code" => {
                if code.replace(value.into_owned()).is_some() {
                    bail!("Duplicate OpenAI authorization code");
                }
            }
            "error" => rejected = true,
            _ => {}
        }
    }
    if expected_state.is_empty() || state.as_deref() != Some(expected_state) {
        bail!("OpenAI login state mismatch; use the URL from this login attempt");
    }
    if rejected {
        bail!("OpenAI login was denied or failed; start a new login attempt");
    }
    code.filter(|code| !code.trim().is_empty())
        .context("OpenAI login callback contains no authorization code")
}

fn jwt_account_id(token: &str) -> Result<String> {
    let payload = token.split('.').nth(1).context("Invalid OpenAI ID token")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .context("Invalid OpenAI ID token")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).context("Invalid OpenAI ID token")?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|value| value.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .context("OpenAI ID token contains no ChatGPT account id")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_mode_does_not_bind_a_callback_listener() -> Result<()> {
        assert!(callback_listener(true)?.is_none());
        Ok(())
    }

    #[test]
    fn accepts_matching_callback_and_decodes_the_code() -> Result<()> {
        let code = code_from_callback_url(
            "http://localhost:1455/auth/callback?code=sample%2Bcode%2Fvalue&state=expected&extra=ignored",
            "expected",
        )?;
        assert_eq!(code, "sample+code/value");
        Ok(())
    }

    #[test]
    fn rejects_invalid_callbacks_without_exposing_secrets() {
        let cases = [
            "not-a-url-private-secret",
            "http://example.com:1455/auth/callback?code=private-secret&state=expected",
            "https://localhost:1455/auth/callback?code=private-secret&state=expected",
            "http://localhost:1456/auth/callback?code=private-secret&state=expected",
            "http://localhost:1455/wrong?code=private-secret&state=expected",
            "http://private-secret@localhost:1455/auth/callback?code=code&state=expected",
            "http://localhost:1455/auth/callback?code=private-secret&state=expected#fragment",
            "http://localhost:1455/auth/callback?code=private-secret",
            "http://localhost:1455/auth/callback?code=private-secret&state=wrong",
            "http://localhost:1455/auth/callback?code=private-secret&state=expected&state=expected",
            "http://localhost:1455/auth/callback?code=private-secret&code=second&state=expected",
            "http://localhost:1455/auth/callback?state=expected",
            "http://localhost:1455/auth/callback?state=expected&code=",
            "http://localhost:1455/auth/callback?state=expected&error=private-secret&error_description=private-secret",
        ];
        for callback in cases {
            let result = code_from_callback_url(callback, "expected");
            assert!(result.is_err());
            if let Err(error) = result {
                assert!(!format!("{error:#}").contains("private-secret"));
            }
        }
    }
}
