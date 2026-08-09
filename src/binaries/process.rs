//! Cancellable child-process execution with concurrent stdout/stderr draining.

use std::{
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    sync::mpsc,
};

use super::{environment::BuildEnvironment, BuildEvent};

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("build cancelled")]
    Cancelled,
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("failed while waiting for {program}: {source}")]
    Wait {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} exited with {status}")]
    Failed { program: String, status: String },
}

pub async fn run(
    program: &str,
    arguments: &[String],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
    event_tx: &Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), CommandError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::Cancelled);
    }

    emit_log(
        event_tx,
        log_tx,
        format!("\n$ {}\n", command_display(program, arguments)),
    )
    .await;

    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }

    let mut child = command.spawn().map_err(|source| CommandError::Spawn {
        program: program.to_owned(),
        source,
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut stdout_task = stdout
        .map(|reader| tokio::spawn(drain(reader, event_tx.clone(), log_tx.clone(), "stdout")));
    let mut stderr_task = stderr
        .map(|reader| tokio::spawn(drain(reader, event_tx.clone(), log_tx.clone(), "stderr")));

    let mut poll = tokio::time::interval(Duration::from_millis(200));
    let status = loop {
        tokio::select! {
            result = child.wait() => {
                break result.map_err(|source| CommandError::Wait {
                    program: program.to_owned(),
                    source,
                })?;
            }
            _ = poll.tick() => {
                if cancelled.load(Ordering::Acquire) {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    if let Some(task) = stdout_task.take() {
                        let _ = task.await;
                    }
                    if let Some(task) = stderr_task.take() {
                        let _ = task.await;
                    }
                    return Err(CommandError::Cancelled);
                }
            }
        }
    };

    if let Some(task) = stdout_task.take() {
        let _ = task.await;
    }
    if let Some(task) = stderr_task.take() {
        let _ = task.await;
    }

    if status.success() {
        Ok(())
    } else {
        Err(CommandError::Failed {
            program: program.to_owned(),
            status: status
                .code()
                .map_or_else(|| "a signal".to_owned(), |code| format!("status {code}")),
        })
    }
}

pub async fn probe(
    program: &str,
    arguments: &[&str],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .kill_on_drop(true);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    let output = command.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|output| output.trim().to_owned())
}

async fn drain<R>(
    mut reader: R,
    event_tx: Sender<BuildEvent>,
    log_tx: mpsc::Sender<String>,
    stream_name: &'static str,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let text = String::from_utf8_lossy(&buffer[..read]);
                emit_log(&event_tx, &log_tx, normalise_output(&text)).await;
            }
            Err(error) => {
                emit_log(
                    &event_tx,
                    &log_tx,
                    format!("\nCould not read build {stream_name}: {error}\n"),
                )
                .await;
                break;
            }
        }
    }
}

pub async fn emit_log(
    event_tx: &Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    message: String,
) {
    let _ = event_tx.send(BuildEvent::Log(message.clone()));
    let _ = log_tx.send(message).await;
}

fn normalise_output(output: &str) -> String {
    output.replace("\r\n", "\n").replace('\r', "\n")
}

fn command_display(program: &str, arguments: &[String]) -> String {
    std::iter::once(program)
        .chain(arguments.iter().map(String::as_str))
        .map(|value| {
            if value.contains(char::is_whitespace) {
                format!("{value:?}")
            } else {
                value.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_display_quotes_whitespace_without_shell_expansion() {
        let display = command_display("git", &["clone".to_owned(), "/tmp/a build".to_owned()]);
        assert_eq!(display, "git clone \"/tmp/a build\"");
    }

    #[test]
    fn carriage_return_progress_becomes_readable_lines() {
        assert_eq!(normalise_output("one\rtwo\r\nthree"), "one\ntwo\nthree");
    }

    #[tokio::test]
    async fn failed_compiler_process_is_reported() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let (log_tx, _log_rx) = mpsc::channel(8);
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = run(
            "/usr/bin/false",
            &[],
            None,
            &environment,
            &event_tx,
            &log_tx,
            &cancelled,
        )
        .await;
        assert!(matches!(result, Err(CommandError::Failed { .. })));
    }
}
