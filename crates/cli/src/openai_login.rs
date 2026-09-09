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
};

#[derive(Deserialize)]
#[allow(clippy::struct_field_names)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    id_token: String,
}

pub async fn run() -> Result<()> {
    let mut verifier_bytes = [0_u8; 64];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut state_bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut state_bytes);
    let state = URL_SAFE_NO_PAD.encode(state_bytes);
    let listener = TcpListener::bind(("127.0.0.1", 1455))
        .context("Binding OpenAI login callback on port 1455")?;
    let redirect = "http://localhost:1455/auth/callback";
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
    println!("Opening ChatGPT login in your browser…");
    if std::process::Command::new("xdg-open")
        .arg(&url)
        .status()
        .is_err()
    {
        println!("Open this URL manually:\n{url}");
    }
    let (mut stream, _) = tokio::task::spawn_blocking(move || listener.accept()).await??;
    let mut data = [0_u8; 16384];
    let length = stream.read(&mut data)?;
    let request = std::str::from_utf8(data.get(..length).context("Invalid callback request")?)?;
    let target = request
        .split_whitespace()
        .nth(1)
        .context("Invalid callback request")?;
    let callback = reqwest::Url::parse(&format!("http://localhost{target}"))?;
    let params = callback
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    if params
        .get("state")
        .is_none_or(|value| value != state.as_str())
    {
        bail!("OpenAI login state mismatch");
    }
    let code = params
        .get("code")
        .context("OpenAI login callback contains no authorization code")?;
    let response = reqwest::Client::new()
        .post(format!("{}/oauth/token", codex::ISSUER))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_ref()),
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
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<h1>Login complete</h1><p>You can return to ri.</p>")?;
    println!("OpenAI Codex login saved to {}", path.display());
    Ok(())
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
