//! Private, immutable context archives. A session without persistence uses a temporary directory.
use anyhow::{Context, Result};
use std::{io::Write as _, path::PathBuf};

pub struct Artifacts {
    directory: PathBuf,
    _temporary: Option<tempfile::TempDir>,
}

impl Artifacts {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        let temporary = if directory.is_none() {
            Some(tempfile::tempdir()?)
        } else {
            None
        };
        let directory = directory
            .or_else(|| temporary.as_ref().map(|value| value.path().to_owned()))
            .context("Missing artifact directory")?;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        Ok(Self {
            directory,
            _temporary: temporary,
        })
    }

    pub async fn save(&self, content: String) -> Result<PathBuf> {
        let path = self.directory.join(format!("{}.txt", uuid::Uuid::new_v4()));
        tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            Ok(path)
        })
        .await
        .context("Saving context artifact")?
    }
}
