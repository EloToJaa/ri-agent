//! Working-directory-scoped `SQLite` persistence. API credentials are never serialized.
use crate::{agent::Selection, message::Message};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone)]
pub struct SessionStore {
    path: PathBuf,
    cwd: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedSession {
    pub id: String,
    #[serde(default = "legacy_provider")]
    pub provider: String,
    pub selection: Selection,
    pub interrupted: bool,
    #[serde(default)]
    pub status: SessionStatus,
    pub(crate) messages: Vec<Message>,
    pub(crate) stable_len: usize,
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Ready,
    Running,
    Failed,
    Paused,
}

/// Durable intent and results for the currently executing model response.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ToolJournal {
    pub messages: Vec<Message>,
    pub calls: Vec<JournalCall>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct JournalCall {
    pub id: String,
    pub started: bool,
    pub result: Option<String>,
}

impl ToolJournal {
    pub fn recover(self) -> Vec<Message> {
        let mut messages = self.messages;
        for call in self.calls {
            messages.push(Message::Tool {
                tool_call_id: call.id,
                content: call.result.unwrap_or_else(|| {
                    if call.started {
                        serde_json::json!({"execution":"unknown", "error":"Execution was interrupted before its result was saved. Inspect local state before retrying; effects may already exist."}).to_string()
                    } else {
                        serde_json::json!({"executed":false, "error":"Tool was not started before the turn stopped."}).to_string()
                    }
                }),
            });
        }
        messages
    }
}

fn legacy_provider() -> String {
    "openrouter".into()
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: String,
    pub interrupted: bool,
}

impl SessionStore {
    pub(crate) fn artifact_directory(&self) -> PathBuf {
        self.path.with_extension("artifacts")
    }
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::config::harness_directory()?.join("sessions.sqlite3"))
    }

    pub fn open(path: PathBuf, cwd: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(parent)
            .context("Creating session database directory")?;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("Creating session database"),
        }
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            bail!("Session database must not be a symbolic link");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        let store = Self {
            path,
            cwd: cwd
                .canonicalize()?
                .to_str()
                .context("Session working directory must be UTF-8")?
                .to_owned(),
        };
        let connection = store.connect()?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY, cwd TEXT NOT NULL, title TEXT NOT NULL,
                model TEXT NOT NULL, interrupted INTEGER NOT NULL,
                payload TEXT NOT NULL, revision INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%d %H:%M:%f','now'))
            );
            CREATE INDEX IF NOT EXISTS sessions_cwd_updated ON sessions(cwd, updated_at DESC);
            CREATE TABLE IF NOT EXISTS tool_journals (
                session_id TEXT PRIMARY KEY, payload TEXT NOT NULL
            );",
        )?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path).context("Opening session database")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(connection)
    }

    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let connection = store.connect()?;
            let mut statement = connection.prepare("SELECT id,title,model,updated_at,interrupted FROM sessions WHERE cwd=?1 ORDER BY updated_at DESC, id DESC")?;
            let rows = statement.query_map([&store.cwd], |row| Ok(SessionSummary {
                id: row.get(0)?, title: row.get(1)?, model: row.get(2)?, updated_at: row.get(3)?, interrupted: row.get(4)?,
            }))?;
            rows.collect::<Result<Vec<_>, _>>().context("Listing saved sessions")
        }).await.context("Session database task failed")?
    }

    pub async fn load(&self, id: String) -> Result<SavedSession> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let connection = store.connect()?;
            let (payload, revision): (String, i64) = connection
                .query_row(
                    "SELECT payload,revision FROM sessions WHERE cwd=?1 AND id=?2",
                    params![store.cwd, id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .context("Session not found in this working directory")?;
            let mut saved: SavedSession =
                serde_json::from_str(&payload).context("Invalid saved session")?;
            if saved.id != id || saved.stable_len > saved.messages.len() {
                bail!("Invalid saved session checkpoint");
            }
            saved.revision = revision;
            Ok(saved)
        })
        .await
        .context("Session database task failed")?
    }

    pub(crate) async fn save(&self, saved: SavedSession) -> Result<i64> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = store.connect()?;
            let transaction = connection.transaction()?;
            let title = saved.messages.iter().find_map(|message| match message {
                Message::User { content } => Some(content.chars().filter(|c| !c.is_control()).take(100).collect::<String>()),
                _ => None,
            }).unwrap_or_else(|| "New conversation".into());
            let payload = serde_json::to_string(&saved)?;
            let revision = saved.revision.checked_add(1).context("Session revision exhausted")?;
            let changes = transaction.execute("INSERT INTO sessions (id,cwd,title,model,interrupted,payload,revision)
                VALUES (?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(id) DO UPDATE SET title=excluded.title,model=excluded.model,
                    interrupted=excluded.interrupted,payload=excluded.payload,revision=excluded.revision,
                    updated_at=strftime('%Y-%m-%d %H:%M:%f','now')
                WHERE sessions.cwd=?2 AND sessions.revision=?8",
                params![saved.id,store.cwd,title,saved.selection.model,saved.interrupted,payload,revision,saved.revision])?;
            if changes != 1 { bail!("Session changed in another process; resume it again before continuing"); }
            transaction.execute("DELETE FROM tool_journals WHERE session_id=?1", [&saved.id])?;
            transaction.commit()?;
            Ok(revision)
        }).await.context("Session database task failed")?
    }

    pub(crate) async fn journal(
        &self,
        id: String,
        revision: i64,
        journal: ToolJournal,
    ) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = store.connect()?;
            let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let current: i64 = transaction.query_row("SELECT revision FROM sessions WHERE id=?1 AND cwd=?2", params![id, store.cwd], |row| row.get(0))?;
            if current != revision {
                bail!("Session changed in another process; refusing tool execution");
            }
            transaction.execute("INSERT INTO tool_journals(session_id,payload) VALUES (?1,?2) ON CONFLICT(session_id) DO UPDATE SET payload=excluded.payload", params![id, serde_json::to_string(&journal)?])?;
            transaction.commit()?;
            Ok(())
        }).await.context("Tool journal task failed")?
    }

    pub(crate) async fn load_journal(&self, id: String) -> Result<Option<ToolJournal>> {
        use rusqlite::OptionalExtension as _;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let connection = store.connect()?;
            let payload: Option<String> = connection.query_row("SELECT j.payload FROM tool_journals j JOIN sessions s ON s.id=j.session_id WHERE s.id=?1 AND s.cwd=?2", params![id, store.cwd], |row| row.get(0)).optional()?;
            payload.map(|value| serde_json::from_str(&value).context("Invalid tool journal")).transpose()
        }).await.context("Tool journal task failed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_sessions_scopes_projects_and_detects_conflicts() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ri-agent-sqlite-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(directory.join("other"))?;
        let path = directory.join("sessions.sqlite3");
        let store = SessionStore::open(path.clone(), &directory)?;
        let saved = SavedSession {
            id: "test".into(),
            provider: "openrouter".into(),
            selection: Selection {
                model: "mock".into(),
                reasoning_effort: None,
            },
            interrupted: true,
            status: SessionStatus::Running,
            messages: vec![Message::User {
                content: "hello".into(),
            }],
            stable_len: 0,
            revision: 0,
        };
        assert_eq!(store.save(saved.clone()).await?, 1);
        assert!(store.save(saved).await.is_err());
        let restored = store.load("test".into()).await?;
        assert_eq!(restored.messages.len(), 1);
        assert!(restored.interrupted);
        assert_eq!(store.list().await?.len(), 1);
        let other = SessionStore::open(path, &directory.join("other"))?;
        assert!(other.list().await?.is_empty());
        assert!(other.load("test".into()).await.is_err());
        fs::remove_dir_all(directory)?;
        Ok(())
    }
}
