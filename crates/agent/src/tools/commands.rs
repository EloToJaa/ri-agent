//! Session-owned subprocesses; dropping the manager cancels all running commands.
use crate::{
    cancellation::Cancellation,
    events::{Event, Output},
    limits::Limits,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Notify,
};

#[derive(Default)]
pub struct Commands(Mutex<HashMap<String, Arc<Job>>>);

struct Job {
    state: Mutex<State>,
    changed: Notify,
    cancel: Cancellation,
}

#[derive(Default)]
struct State {
    stdout: VecDeque<u8>,
    stderr: VecDeque<u8>,
    truncated: bool,
    done: bool,
    exit_code: Option<i32>,
    error: Option<String>,
    reason: StopReason,
    emitted: usize,
}

#[derive(Default, PartialEq, Eq)]
enum StopReason {
    #[default]
    Exited,
    Cancelled,
    TimedOut,
}

#[derive(Serialize)]
#[allow(clippy::struct_excessive_bools)] // Stable, independently useful JSON result flags.
pub struct CommandResult {
    pub command_id: String,
    pub running: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub cancelled: bool,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct ProcessGroup {
    #[cfg(unix)]
    pid: Option<rustix::process::Pid>,
}

impl ProcessGroup {
    fn new(id: u32) -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                pid: Some(
                    rustix::process::Pid::from_raw(i32::try_from(id)?)
                        .context("Invalid process group")?,
                ),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = id;
            Ok(Self {})
        }
    }
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.take() {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        for job in self
            .0
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            job.cancel.cancel();
        }
    }
}

impl Commands {
    pub fn start(
        &self,
        command: &str,
        cwd: Option<&std::path::Path>,
        limits: Limits,
        output: &Output,
    ) -> Result<String> {
        let mut jobs = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Command registry poisoned"))?;
        if jobs.len() >= 64 {
            jobs.retain(|_, job| {
                !job.state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .done
            });
            if jobs.len() >= 64 {
                bail!("Too many running commands; cancel one before starting another");
            }
        }
        let mut builder = Command::new("bash");
        builder
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        }
        #[cfg(unix)]
        builder.process_group(0);
        let mut child = builder.spawn().context("Failed to execute Bash command")?;
        let mut group = ProcessGroup::new(child.id().context("Missing command PID")?)?;
        let stdout = child.stdout.take().context("Missing Bash stdout")?;
        let stderr = child.stderr.take().context("Missing Bash stderr")?;
        let id = uuid::Uuid::new_v4().to_string();
        let job = Arc::new(Job {
            state: Mutex::default(),
            changed: Notify::new(),
            cancel: Cancellation::default(),
        });
        jobs.insert(id.clone(), job.clone());
        drop(jobs);
        let output = output.clone();
        let command_id = id.clone();
        tokio::spawn(async move {
            let stopped = Cancellation::default();
            let read_stdout = drain(
                stdout,
                job.clone(),
                command_id.clone(),
                false,
                limits,
                output.clone(),
            );
            let read_stderr = drain(stderr, job.clone(), command_id, true, limits, output);
            let supervise = async {
                let (status, reason) = tokio::select! {
                    status = child.wait() => (status, StopReason::Exited),
                    () = job.cancel.cancelled() => {
                        group.kill(); let _ = child.start_kill(); (child.wait().await, StopReason::Cancelled)
                    },
                    () = tokio::time::sleep(limits.command_timeout) => {
                        group.kill(); let _ = child.start_kill(); (child.wait().await, StopReason::TimedOut)
                    },
                };
                // A shell's background children must not outlive the command.
                group.kill();
                stopped.cancel();
                let mut state = job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.reason = reason;
                match status {
                    Ok(status) => state.exit_code = status.code(),
                    Err(error) => state.error = Some(error.to_string()),
                }
            };
            let drain = async {
                tokio::select! {
                    result = async { tokio::try_join!(read_stdout, read_stderr) } => result.map(|_| ()),
                    () = async { stopped.cancelled().await; tokio::time::sleep(Duration::from_secs(1)).await; } => Err(anyhow::anyhow!("Command pipes remained open after process exit")),
                }
            };
            let ((), drained) = tokio::join!(supervise, drain);
            let mut state = job
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(error) = drained {
                state.error = Some(error.to_string());
            }
            state.done = true;
            drop(state);
            job.changed.notify_one();
        });
        Ok(id)
    }

    pub async fn poll(
        &self,
        id: &str,
        wait: Duration,
        cancel: bool,
        turn: &Cancellation,
    ) -> Result<CommandResult> {
        let job = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Command registry poisoned"))?
            .get(id)
            .cloned()
            .context(
                "Unknown command ID; command handles belong to the current process and session",
            )?;
        if cancel {
            job.cancel.cancel();
        }
        let completed = async {
            loop {
                let changed = job.changed.notified();
                if job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .done
                {
                    break;
                }
                changed.await;
            }
        };
        tokio::select! {
            () = completed => {},
            () = turn.cancelled() => { job.cancel.cancel(); },
            () = tokio::time::sleep(wait) => {},
        }
        // Cancellation must finish cleanup before subsequent mutations execute.
        if job.cancel.is_cancelled() {
            loop {
                let changed = job.changed.notified();
                if job
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .done
                {
                    break;
                }
                changed.await;
            }
        }
        let mut state = job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stdout =
            String::from_utf8_lossy(&state.stdout.drain(..).collect::<Vec<_>>()).into_owned();
        let mut stderr =
            String::from_utf8_lossy(&state.stderr.drain(..).collect::<Vec<_>>()).into_owned();
        if state.truncated {
            stdout.push_str("\n[output truncated]");
            stderr.push_str("\n[output truncated]");
            state.truncated = false;
        }
        Ok(CommandResult {
            command_id: id.into(),
            running: !state.done,
            stdout,
            stderr,
            exit_code: state.exit_code,
            success: state.done
                && state.exit_code == Some(0)
                && state.reason == StopReason::Exited
                && state.error.is_none(),
            cancelled: state.reason == StopReason::Cancelled,
            timed_out: state.reason == StopReason::TimedOut,
            error: state.error.clone(),
        })
    }
}

async fn drain(
    mut reader: impl AsyncRead + Unpin,
    job: Arc<Job>,
    id: String,
    stderr: bool,
    limits: Limits,
    output: Output,
) -> Result<()> {
    let mut buffer = [0; 4096];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        let bytes = buffer
            .get(..count)
            .context("Invalid command output length")?;
        let mut state = job
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stream = if stderr {
            &mut state.stderr
        } else {
            &mut state.stdout
        };
        let keep = bytes
            .len()
            .min(limits.max_output_bytes.get().saturating_sub(stream.len()));
        stream.extend(bytes.iter().take(keep));
        state.truncated |= keep < bytes.len();
        let emit = bytes
            .len()
            .min(limits.max_output_bytes.get().saturating_sub(state.emitted));
        state.emitted += emit;
        drop(state);
        if emit > 0 {
            output.emit(Event::CommandOutput {
                command_id: id.clone(),
                stream: if stderr { "stderr" } else { "stdout" }.into(),
                text: String::from_utf8_lossy(bytes.get(..emit).unwrap_or_default()).into_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streams_before_exit_polls_incrementally_and_isolates_handles() -> Result<()> {
        let manager = Commands::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let id = manager.start(
            "printf first; sleep 0.1; printf second",
            None,
            Limits::default(),
            &Output::channel(sender),
        )?;
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await?
            .context("missing stream event")?;
        assert!(matches!(event, Event::CommandOutput { text, .. } if text == "first"));
        let cancellation = Cancellation::default();
        let first = manager
            .poll(&id, Duration::ZERO, false, &cancellation)
            .await?;
        assert!(first.running);
        assert_eq!(first.stdout, "first");
        assert!(
            Commands::default()
                .poll(&id, Duration::ZERO, false, &cancellation)
                .await
                .is_err()
        );
        let second = manager
            .poll(&id, Duration::from_secs(2), false, &cancellation)
            .await?;
        assert!(!second.running);
        assert!(second.success);
        assert_eq!(second.stdout, "second");
        assert!(
            manager
                .poll(&id, Duration::ZERO, false, &cancellation)
                .await?
                .stdout
                .is_empty()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_descendants_before_they_can_write() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let manager = Commands::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let id = manager.start(
            "(sleep 0.3; printf survived > marker) & printf ready; wait",
            Some(directory.path()),
            Limits::default(),
            &Output::channel(sender),
        )?;
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await?
            .context("ready event")?;
        let result = manager
            .poll(&id, Duration::from_secs(2), true, &Cancellation::default())
            .await?;
        assert!(result.cancelled);
        assert!(!result.running);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!directory.path().join("marker").exists());
        Ok(())
    }
}
