//! Private credential storage, separate from conversation/session persistence.
use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

pub type ApiKey = SecretString;

#[derive(Clone)]
pub struct CodexCredentials {
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    pub id_token: SecretString,
    pub account_id: String,
}

pub fn api_key(value: impl AsRef<str>) -> Result<ApiKey> {
    let value = value.as_ref().trim();
    if value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.chars().any(char::is_control)
    {
        bail!("API key must be nonempty and contain no whitespace or control characters");
    }
    Ok(value.to_owned().into())
}

pub fn default_path() -> Result<PathBuf> {
    Ok(crate::config::harness_directory()?.join("credentials.json"))
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openrouter: Option<StoredKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    openai_codex: Option<StoredCodex>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredKey {
    api_key: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCodex {
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
}

pub fn load(path: &Path) -> Result<Option<ApiKey>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Reading credential file metadata"),
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 65536 {
                bail!("Credential path must be a regular file of at most 64 KiB");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    bail!(
                        "Credential file permissions are too open; run chmod 600 on ~/.ri/credentials.json"
                    );
                }
            }
        }
    }
    let source = fs::read_to_string(path).context("Reading saved credentials")?;
    let credentials: Credentials = serde_json::from_str(&source)
        .map_err(|_| anyhow::anyhow!("Invalid credentials.json format; run 'ri login' again"))?;
    credentials
        .openrouter
        .map(|stored| api_key(stored.api_key))
        .transpose()
}

/// Saved credentials cannot be silently redirected by a Lua endpoint override.
pub fn resolve(base_url: &str) -> Result<ApiKey> {
    match env::var("OPENROUTER_API_KEY") {
        Ok(value) => return api_key(value).context("Invalid OPENROUTER_API_KEY"),
        Err(env::VarError::NotUnicode(_)) => bail!("OPENROUTER_API_KEY is not valid Unicode"),
        Err(env::VarError::NotPresent) => {}
    }
    let key = load(&default_path()?)?
        .context("No OpenRouter credentials found. Run 'ri login' or set OPENROUTER_API_KEY")?;
    if base_url.trim_end_matches('/') != crate::openrouter::DEFAULT_BASE_URL {
        bail!(
            "Saved credentials can only be sent to the official OpenRouter API. For a custom endpoint, explicitly set OPENROUTER_API_KEY"
        );
    }
    Ok(key)
}

/// Atomically replace the saved key; temporary and final files are private on Unix.
pub fn save(path: &Path, key: &ApiKey) -> Result<()> {
    let parent = path.parent().context("Credential path has no parent")?;
    let mut directory = fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory
        .create(parent)
        .context("Creating credential directory")?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("Refusing to replace a non-regular credential file");
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("Creating private credential file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let existing = load_credentials_unchecked(path).unwrap_or_default();
    let credentials = Credentials {
        openrouter: Some(StoredKey {
            api_key: key.expose_secret().to_owned(),
        }),
        openai_codex: existing.openai_codex,
    };
    let data = serde_json::to_vec(&credentials).context("Serializing credentials")?;
    temporary.write_all(&data)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Saving credentials")?;
    Ok(())
}

fn load_credentials_unchecked(path: &Path) -> Option<Credentials> {
    fs::read_to_string(path)
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
}

pub fn load_codex(path: &Path) -> Result<Option<CodexCredentials>> {
    validate_path(path)?;
    let Some(stored) =
        load_credentials_unchecked(path).and_then(|credentials| credentials.openai_codex)
    else {
        return Ok(None);
    };
    if stored.account_id.trim().is_empty() {
        bail!("Invalid OpenAI Codex credentials; run 'ri login openai-codex' again");
    }
    Ok(Some(CodexCredentials {
        access_token: stored.access_token.into(),
        refresh_token: stored.refresh_token.into(),
        id_token: stored.id_token.into(),
        account_id: stored.account_id,
    }))
}

pub fn save_codex(path: &Path, value: &CodexCredentials) -> Result<()> {
    validate_path(path)?;
    let mut credentials = load_credentials_unchecked(path).unwrap_or_default();
    credentials.openai_codex = Some(StoredCodex {
        access_token: value.access_token.expose_secret().to_owned(),
        refresh_token: value.refresh_token.expose_secret().to_owned(),
        id_token: value.id_token.expose_secret().to_owned(),
        account_id: value.account_id.clone(),
    });
    write_credentials(path, &credentials)
}

fn validate_path(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 65536)
    {
        bail!("Credential path must be a regular file of at most 64 KiB");
    }
    Ok(())
}

fn write_credentials(path: &Path, credentials: &Credentials) -> Result<()> {
    let parent = path.parent().context("Credential path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(&serde_json::to_vec(credentials)?)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_replaces_keys_without_leaking_them_in_debug_output() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(".ri/credentials.json");
        assert!(load(&path)?.is_none());
        let key = api_key("mock-secret-one")?;
        assert!(!format!("{key:?}").contains("mock-secret"));
        save(&path, &key)?;
        save(&path, &api_key("mock-secret-two")?)?;
        assert_eq!(
            load(&path)?.context("Missing saved key")?.expose_secret(),
            "mock-secret-two"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_keys_and_malformed_storage() -> Result<()> {
        for value in ["", "  ", "key with spaces", "key\nother"] {
            assert!(api_key(value).is_err());
        }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        save(&path, &api_key("mock")?)?;
        fs::write(&path, "not-json-containing-a-secret")?;
        let error = load(&path).err().context("Expected an error")?;
        assert!(!format!("{error:#}").contains("containing-a-secret"));
        Ok(())
    }
}
