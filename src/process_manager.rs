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
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use libc;

// ── Thread-safe output queue ─────────────────────────────────────────────────

/// Lines produced by a child process, drained by the UI every 100 ms.
pub type OutputQueue = Arc<Mutex<VecDeque<String>>>;

pub fn new_queue() -> OutputQueue {
    Arc::new(Mutex::new(VecDeque::new()))
}

fn push_line(queue: &OutputQueue, line: String) {
    if let Ok(mut q) = queue.lock() {
        // Cap at 10 000 lines to bound memory usage.
        if q.len() > 10_000 {
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

    /// Graceful SIGTERM → 10 s wait → SIGKILL.
    pub fn terminate(&mut self) {
        let pid = self.child.id() as i32;
        // Attempt graceful shutdown with SIGTERM
        unsafe { libc::kill(pid, libc::SIGTERM) };
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
        // Escalate to SIGKILL
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
    queue: OutputQueue,
) -> Result<ProcessHandle> {
    let bitcoind = binaries_path.join("bitcoind");
    if !bitcoind.exists() {
        bail!("bitcoind not found at {}", bitcoind.display());
    }

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("create bitcoin data dir {:?}", data_dir))?;

    let cmd = [
        bitcoind.to_string_lossy().into_owned(),
        format!("-datadir={}", data_dir.display()),
        "-printtoconsole".into(),
    ];

    push_line(&queue, format!("$ {}", cmd.join(" ")));

    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn bitcoind {:?}", bitcoind))?;

    spawn_reader_thread(child, queue)
}

// ── Electrs ───────────────────────────────────────────────────────────────────

/// Launch `electrs` and stream its output into `queue`.
pub fn launch_electrs(
    binaries_path: &Path,
    bitcoin_data_dir: &Path,
    electrs_db_dir: &Path,
    queue: OutputQueue,
) -> Result<ProcessHandle> {
    let electrs = binaries_path.join("electrs");
    if !electrs.exists() {
        bail!("electrs not found at {}", electrs.display());
    }

    std::fs::create_dir_all(electrs_db_dir)
        .with_context(|| format!("create electrs db dir {:?}", electrs_db_dir))?;

    let cmd = [
        electrs.to_string_lossy().into_owned(),
        "--network".into(),
        "bitcoin".into(),
        "--daemon-dir".into(),
        bitcoin_data_dir.to_string_lossy().into_owned(),
        "--db-dir".into(),
        electrs_db_dir.to_string_lossy().into_owned(),
        "--electrum-rpc-addr".into(),
        "127.0.0.1:50001".into(),
    ];

    push_line(&queue, format!("$ {}", cmd.join(" ")));

    let child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn electrs {:?}", electrs))?;

    spawn_reader_thread(child, queue)
}

// ── Reader thread ─────────────────────────────────────────────────────────────

/// Spawn a background thread that reads stdout+stderr from `child` into `queue`.
/// Returns a `ProcessHandle` wrapping the child.
///
/// Both stdout and stderr are read concurrently on separate threads that both
/// push into the same queue, preserving approximate interleaving order.
fn spawn_reader_thread(mut child: Child, queue: OutputQueue) -> Result<ProcessHandle> {
    // Take stdout and stderr pipes before the child is moved into ProcessHandle
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;

    // stdout reader
    {
        let q = Arc::clone(&queue);
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
        let q = Arc::clone(&queue);
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

// ── Sync detection helpers ────────────────────────────────────────────────────

/// Check whether a line from electrs output indicates it is fully synced.
pub fn is_electrs_synced_line(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("finished full compaction")
        || l.contains("electrs running")
        || l.contains("waiting for new block")
        || l.contains("index update completed")
        || l.contains("chain best block")
}
