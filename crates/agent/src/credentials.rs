//! Private credential storage, separate from conversation/session persistence.
use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{env, fs, io::Write, path::{Path, PathBuf}};

pub type ApiKey = SecretString;

pub fn api_key(value: String) -> Result<ApiKey> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        bail!("API key must be nonempty and contain no whitespace or control characters");
    }
    Ok(value.to_owned().into())
}

pub fn default_path() -> Result<PathBuf> {
    Ok(crate::config::harness_directory()?.join("credentials.json"))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Credentials { openrouter: StoredKey }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredKey { api_key: String }

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
                    bail!("Credential file permissions are too open; run chmod 600 on ~/.ri/credentials.json");
                }
            }
        }
    }
    let source = fs::read_to_string(path).context("Reading saved credentials")?;
    let credentials: Credentials = serde_json::from_str(&source).map_err(|_| anyhow::anyhow!("Invalid credentials.json format; run 'ri login' again"))?;
    Ok(Some(api_key(credentials.openrouter.api_key)?))
}

/// Saved credentials cannot be silently redirected by a Lua endpoint override.
pub fn resolve(base_url: &str) -> Result<ApiKey> {
    match env::var("OPENROUTER_API_KEY") {
        Ok(value) => return api_key(value).context("Invalid OPENROUTER_API_KEY"),
        Err(env::VarError::NotUnicode(_)) => bail!("OPENROUTER_API_KEY is not valid Unicode"),
        Err(env::VarError::NotPresent) => {}
    }
    let key = load(&default_path()?)?.context("No OpenRouter credentials found. Run 'ri login' or set OPENROUTER_API_KEY")?;
    if base_url.trim_end_matches('/') != crate::openrouter::DEFAULT_BASE_URL {
        bail!("Saved credentials can only be sent to the official OpenRouter API. For a custom endpoint, explicitly set OPENROUTER_API_KEY");
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
    directory.create(parent).context("Creating credential directory")?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    { bail!("Refusing to replace a non-regular credential file"); }
    let mut temporary = tempfile::NamedTempFile::new_in(parent).context("Creating private credential file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let credentials = Credentials { openrouter:StoredKey { api_key:key.expose_secret().to_owned() } };
    let data = serde_json::to_vec(&credentials).context("Serializing credentials")?;
    temporary.write_all(&data)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error).context("Saving credentials")?;
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
        let key = api_key("mock-secret-one".into())?;
        assert!(!format!("{key:?}").contains("mock-secret"));
        save(&path, &key)?;
        save(&path, &api_key("mock-secret-two".into())?)?;
        assert_eq!(load(&path)?.context("Missing saved key")?.expose_secret(), "mock-secret-two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_keys_and_malformed_storage() -> Result<()> {
        for value in ["", "  ", "key with spaces", "key\nother"] { assert!(api_key(value.into()).is_err()); }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        save(&path, &api_key("mock".into())?)?;
        fs::write(&path, "not-json-containing-a-secret")?;
        let error = load(&path).err().context("Expected an error")?;
        assert!(!format!("{error:#}").contains("containing-a-secret"));
        Ok(())
    }
}
