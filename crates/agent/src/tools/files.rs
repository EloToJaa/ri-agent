//! Prepared edits tie approval to a specific file version and publish atomically.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
};

pub const MAX_EDIT_BYTES: u64 = 16 * 1024 * 1024;

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct Snapshot {
    requested: PathBuf,
    path: PathBuf,
    pub content: Option<String>,
    permissions: Option<std::fs::Permissions>,
}

impl Snapshot {
    pub async fn read(
        requested: PathBuf,
        missing_ok: bool,
        expected: Option<&str>,
    ) -> Result<Self> {
        let (path, content, permissions) = match tokio::fs::symlink_metadata(&requested).await {
            Ok(_) => {
                let path = tokio::fs::canonicalize(&requested).await?;
                let metadata = tokio::fs::metadata(&path).await?;
                if !metadata.is_file() {
                    bail!("Mutation target must be a regular file");
                }
                if metadata.len() > MAX_EDIT_BYTES {
                    bail!("File exceeds 16 MiB edit limit");
                }
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .context("Mutation target must be UTF-8")?;
                (path, Some(content), Some(metadata.permissions()))
            }
            Err(error) if missing_ok && error.kind() == std::io::ErrorKind::NotFound => {
                let parent = requested
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let path = tokio::fs::canonicalize(parent)
                    .await?
                    .join(requested.file_name().context("Missing file name")?);
                (path, None, None)
            }
            Err(error) => return Err(error).context("Reading mutation target"),
        };
        if let Some(expected) = expected {
            let actual = content
                .as_ref()
                .map_or_else(|| "missing".into(), |value| hash(value.as_bytes()));
            if actual != expected {
                bail!("File changed since it was read: expected_sha256 mismatch; read it again");
            }
        }
        Ok(Self {
            requested,
            path,
            content,
            permissions,
        })
    }

    pub fn prepare(self, replacement: String, result: String) -> Result<Mutation> {
        if replacement.len() as u64 > MAX_EDIT_BYTES {
            bail!("Replacement exceeds 16 MiB edit limit");
        }
        Ok(Mutation {
            snapshot: self,
            replacement,
            result,
        })
    }
}

pub struct Mutation {
    snapshot: Snapshot,
    replacement: String,
    pub result: String,
}

impl Mutation {
    pub async fn apply(self) -> Result<String> {
        tokio::task::spawn_blocking(move || {
            let snapshot = self.snapshot;
            let parent = snapshot.path.parent().context("Missing target parent")?;
            let mut staged = tempfile::NamedTempFile::new_in(parent)?;
            if let Some(permissions) = &snapshot.permissions {
                staged.as_file().set_permissions(permissions.clone())?;
            }
            staged.write_all(self.replacement.as_bytes())?;
            staged.as_file().sync_all()?;
            if let Some(original) = snapshot.content {
                if std::fs::canonicalize(&snapshot.requested)? != snapshot.path
                    || std::fs::read_to_string(&snapshot.path)? != original
                    || Some(std::fs::metadata(&snapshot.path)?.permissions())
                        != snapshot.permissions
                {
                    bail!("File changed while preparing or approving the edit; read it again");
                }
                staged
                    .persist(&snapshot.path)
                    .context("Publishing atomic file replacement")?;
            } else {
                staged
                    .persist_noclobber(&snapshot.path)
                    .context("Target appeared before file creation; read it again")?;
            }
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(self.result)
        })
        .await
        .context("Atomic file operation failed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_versions_and_changes_during_approval_preserve_external_edits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("file");
        std::fs::write(&path, "original")?;
        assert!(
            Snapshot::read(path.clone(), false, Some("stale"))
                .await
                .is_err()
        );
        let snapshot = Snapshot::read(path.clone(), false, Some(&hash(b"original"))).await?;
        let mutation = snapshot.prepare("replacement".into(), "done".into())?;
        std::fs::write(&path, "user edit")?;
        assert!(mutation.apply().await.is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "user edit");
        let path = directory.path().join("new");
        let mutation = Snapshot::read(path.clone(), true, Some("missing"))
            .await?
            .prepare("agent".into(), "done".into())?;
        std::fs::write(&path, "user created")?;
        assert!(mutation.apply().await.is_err());
        assert_eq!(std::fs::read_to_string(path)?, "user created");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_replacement_preserves_permissions_and_symlinks() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("target");
        let link = directory.path().join("link");
        std::fs::write(&path, "old")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        symlink(&path, &link)?;
        Snapshot::read(link.clone(), false, None)
            .await?
            .prepare("new".into(), "done".into())?
            .apply()
            .await?;
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&path)?, "new");
        assert_eq!(std::fs::metadata(path)?.permissions().mode() & 0o777, 0o755);
        Ok(())
    }
}
