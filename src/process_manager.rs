//! Child process management for `bitcoind` and `electrs`.
//!
//! This module spawns processes, streams their combined stdout+stderr into
//! thread-safe queues, and provides graceful shutdown with kill fallback.
//!
//! Design decision: plain OS threads (not Tokio tasks) are used for the stdout
//! reader loops because `std::process::Child` and its `BufReader` are
//! synchronous and blocking reads are fine in a dedicated thread.  The UI
//! drains the queues on a 100 ms timer (see `ui.rs`).

use std::{
    collections::VecDeque,
    io::{BufRead as _, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};

use crate::platform;

// ── Thread-safe output queue ─────────────────────────────────────────────────

/// Lines produced by a child process, drained by the UI every 100 ms.
pub type OutputQueue = Arc<Mutex<VecDeque<String>>>;

pub fn new_queue() -> OutputQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

fn push_line(queue: &OutputQueue, line: String) {
    if let Ok(mut q) = queue.lock() {
        // Cap at 10 000 lines to bound memory usage.
        if q.len() >= 10_000 {
            q.pop_front();
        }
        q.push_back(line);
    }
}

// ── ProcessHandle ────────────────────────────────────────────────────────────

/// Wraps a running child process and its associated reader thread.
pub struct ProcessHandle {
    pub child: Child,
}

impl ProcessHandle {
    /// Returns `true` if the process is still alive.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Graceful termination request → 10 s wait → kill fallback.
    pub fn terminate(&mut self) {
        platform::terminate_child(&self.child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => thread::sleep(Duration::from_millis(200)),
            }
        }
        // Escalate to the platform kill fallback.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Bitcoin ───────────────────────────────────────────────────────────────────

/// Launch `bitcoind` and stream its output into `queue`.
///
/// Returns a handle to the spawned process and starts a background reader thread.
pub fn launch_bitcoind(
    binaries_path: &Path,
    data_dir: &Path,
    queue: &OutputQueue,
) -> Result<ProcessHandle> {
    let bitcoind =
        validated_managed_executable(binaries_path, &platform::executable_name("bitcoind"))?;
    let data_dir = platform::prepare_real_directory(data_dir, "Bitcoin data directory", true)?;

    let args = [
        format!("-datadir={}", data_dir.display()),
        "-printtoconsole".into(),
    ];

    push_line(
        queue,
        format!("$ {}", platform::command_display(&bitcoind, &args)),
    );

    let child = Command::new(&bitcoind)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn bitcoind {}", bitcoind.display()))?;

    spawn_reader_thread(child, queue)
}

// ── Electrs ───────────────────────────────────────────────────────────────────

/// Launch `electrs` and stream its output into `queue`.
pub fn launch_electrs(
    binaries_path: &Path,
    bitcoin_data_dir: &Path,
    electrs_db_dir: &Path,
    electrum_addr: &str,
    queue: &OutputQueue,
) -> Result<ProcessHandle> {
    let electrs = validated_managed_executable(binaries_path, &platform::electrs_binary_name())?;
    let bitcoin_data_dir =
        platform::prepare_real_directory(bitcoin_data_dir, "Bitcoin data directory", false)?;
    let electrs_db_dir =
        platform::prepare_real_directory(electrs_db_dir, "electrs database directory", true)?;

    let args = [
        "--network".into(),
        "bitcoin".into(),
        "--daemon-dir".into(),
        bitcoin_data_dir.to_string_lossy().into_owned(),
        "--db-dir".into(),
        electrs_db_dir.to_string_lossy().into_owned(),
        "--electrum-rpc-addr".into(),
        electrum_addr.to_owned(),
    ];

    push_line(
        queue,
        format!("$ {}", platform::command_display(&electrs, &args)),
    );

    let child = Command::new(&electrs)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn electrs {}", electrs.display()))?;

    spawn_reader_thread(child, queue)
}

fn validated_managed_executable(binaries_path: &Path, name: &str) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let root = platform::prepare_real_directory(binaries_path, "binaries directory", false)?;
    let executable = root.join(name);
    let metadata = std::fs::symlink_metadata(&executable)
        .with_context(|| format!("inspect managed executable {}", executable.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
    {
        bail!(
            "managed executable must be a real executable file: {}",
            executable.display()
        );
    }
    let canonical = std::fs::canonicalize(&executable)
        .with_context(|| format!("canonicalize managed executable {}", executable.display()))?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "managed executable escaped the binaries directory: {}",
            executable.display()
        );
    }
    Ok(canonical)
}

// ── Reader thread ─────────────────────────────────────────────────────────────

/// Spawn a background thread that reads stdout+stderr from `child` into `queue`.
/// Returns a `ProcessHandle` wrapping the child.
///
/// Both stdout and stderr are read concurrently on separate threads that both
/// push into the same queue, preserving approximate interleaving order.
fn spawn_reader_thread(mut child: Child, queue: &OutputQueue) -> Result<ProcessHandle> {
    // Take stdout and stderr pipes before the child is moved into ProcessHandle
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;

    // stdout reader
    {
        let q = Arc::clone(queue);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => push_line(&q, l),
                    Err(_) => break,
                }
            }
        });
    }

    // stderr reader
    {
        let q = Arc::clone(queue);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(l) => push_line(&q, l),
                    Err(_) => break,
                }
            }
        });
    }

    Ok(ProcessHandle { child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn node_launch_rejects_symlinked_executables_without_running_them() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temporary = tempfile::tempdir()?;
        let binaries = temporary.path().join("Binaries");
        let bitcoin_data = temporary.path().join("BitcoinChain");
        let electrs_data = temporary.path().join("ElectrsDB");
        let marker = temporary.path().join("executed");
        let helper = temporary.path().join("helper");
        std::fs::create_dir(&binaries)?;
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::write(
            &helper,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755))?;
        symlink(&helper, binaries.join("bitcoind"))?;
        symlink(&helper, binaries.join("electrs"))?;
        let queue = new_queue();

        assert!(launch_bitcoind(&binaries, &bitcoin_data, &queue).is_err());
        assert!(launch_electrs(
            &binaries,
            &bitcoin_data,
            &electrs_data,
            "127.0.0.1:50001",
            &queue
        )
        .is_err());
        assert!(!marker.exists());
        Ok(())
    }
}
