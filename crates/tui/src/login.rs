//! Give the interactive login process exclusive ownership of the terminal.
use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use std::{future::Future, io, process::Stdio};

/// The outer result reports restoration failure; the inner result is the login outcome.
pub async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    provider: &str,
    manual: bool,
) -> Result<Result<()>> {
    // Poll once to install the SIGINT handler before leaving raw mode. Otherwise Ctrl+C
    // reaches both foreground processes and could terminate ri before it can restore the TUI.
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    if let std::task::Poll::Ready(result) = futures_util::poll!(interrupt.as_mut()) {
        result.context("Installing login interrupt handler")?;
        return Ok(Err(anyhow::anyhow!("Login cancelled")));
    }
    suspended(
        || {
            crossterm::execute!(
                io::stdout(),
                DisableBracketedPaste,
                Show,
                LeaveAlternateScreen
            )?;
            disable_raw_mode().context("Leaving raw mode for login")
        },
        async {
            let mut command = tokio::process::Command::new(
                std::env::current_exe().context("Finding ri executable")?,
            );
            command.arg("login").arg("--provider").arg(provider);
            if manual {
                command.arg("--manual");
            }
            run_command(command, interrupt).await
        },
        || {
            enable_raw_mode().context("Restoring raw mode after login")?;
            crossterm::execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableBracketedPaste,
                Hide
            )?;
            terminal.clear().context("Redrawing terminal after login")
        },
    )
    .await
}

async fn suspended<T>(
    suspend: impl FnOnce() -> Result<()>,
    operation: impl Future<Output = Result<T>>,
    resume: impl FnOnce() -> Result<()>,
) -> Result<Result<T>> {
    let result = match suspend() {
        Ok(()) => operation.await,
        Err(error) => Err(error),
    };
    // Always attempt restoration, including partial suspension and subprocess spawn failures.
    resume().context("Restoring the TUI after login")?;
    Ok(result)
}

async fn run_command(
    mut command: tokio::process::Command,
    interrupt: impl Future<Output = io::Result<()>>,
) -> Result<()> {
    let mut child = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("Running ri login")?;
    let status = tokio::select! {
        status = child.wait() => status.context("Waiting for ri login")?,
        interrupted = interrupt => {
            // Reap the child before resuming the input reader or drawing another frame.
            child.kill().await.context("Stopping ri login")?;
            interrupted.context("Receiving login interrupt")?;
            bail!("Login cancelled");
        }
    };
    if !status.success() {
        bail!("ri login exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};

    #[tokio::test]
    async fn restores_after_success_failure_and_partial_suspension() -> Result<()> {
        for (suspend_ok, login_ok) in [(true, true), (true, false), (false, true)] {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let result = suspended(
                || {
                    calls.borrow_mut().push("suspend");
                    if !suspend_ok {
                        bail!("suspend failed");
                    }
                    Ok(())
                },
                async {
                    calls.borrow_mut().push("login");
                    if !login_ok {
                        bail!("login failed");
                    }
                    Ok(())
                },
                || {
                    calls.borrow_mut().push("resume");
                    Ok(())
                },
            )
            .await?;
            assert_eq!(result.is_ok(), suspend_ok && login_ok);
            assert_eq!(
                *calls.borrow(),
                if suspend_ok {
                    vec!["suspend", "login", "resume"]
                } else {
                    vec!["suspend", "resume"]
                }
            );
        }
        assert!(
            suspended(|| Ok(()), async { Ok(()) }, || bail!("resume failed"))
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn handles_subprocess_success_failure_spawn_failure_and_cancellation() -> Result<()> {
        for (script, success) in [("exit 0", true), ("exit 7", false)] {
            let mut command = tokio::process::Command::new("bash");
            command.args(["-c", script]);
            assert_eq!(
                run_command(command, std::future::pending()).await.is_ok(),
                success
            );
        }
        let missing = tokio::process::Command::new("/nonexistent-ri-login-executable");
        assert!(run_command(missing, std::future::pending()).await.is_err());
        let mut command = tokio::process::Command::new("bash");
        command.args(["-c", "exec sleep 30"]);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_command(command, async { Ok(()) }),
        )
        .await?;
        assert!(
            result
                .err()
                .context("Expected cancellation")?
                .to_string()
                .contains("cancelled")
        );
        Ok(())
    }
}
