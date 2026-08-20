//! Child process management for `bitcoind` and `electrs`.
//!
//! This module spawns processes, streams their combined stdout+stderr into
//! thread-safe queues, and provides graceful shutdown with kill fallback.
//!
//! Design decision: plain OS threads (not Tokio tasks) are used for the stdout
//! reader loops. Each thread polls its synchronous pipe with a cancellation
//! interval so cleanup cannot be held open by an inherited writer. The UI
//! drains the queues on a 100 ms timer (see `ui.rs`).

use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _, Result};

use crate::{connection::ElectrsListenAddr, platform};

#[cfg(test)]
use crate::connection::ElectrsBindPolicy;

const TERMINATION_GRACE_PERIOD: Duration = Duration::from_secs(10);
const TERMINATION_POLL_INTERVAL: Duration = Duration::from_millis(25);
const READER_DRAIN_GRACE_PERIOD: Duration = Duration::from_millis(250);
const READER_POLL_INTERVAL_MILLIS: libc::c_int = 25;
const MAX_BUFFERED_OUTPUT_BYTES: usize = 64 * 1024;

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

/// Owns a managed child process, its process group, and both output readers.
pub struct ProcessHandle {
    child: Option<Child>,
    process_group_id: Option<libc::pid_t>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    reader_cancel: Arc<AtomicBool>,
    exit_diagnostic: Option<String>,
}

impl ProcessHandle {
    fn new(child: Child) -> Self {
        let process_group_id = libc::pid_t::try_from(child.id()).ok();
        Self {
            child: Some(child),
            process_group_id,
            stdout_reader: None,
            stderr_reader: None,
            reader_cancel: Arc::new(AtomicBool::new(false)),
            exit_diagnostic: None,
        }
    }

    /// Returns `true` if the process is still alive.
    pub fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };

        match child_has_exited(child) {
            Ok(false) => true,
            Ok(true) => {
                // Keep the exited leader waitable until cleanup has disarmed
                // its process group. This prevents the numeric PID/PGID from
                // being reused before the group signal is sent.
                self.force_cleanup_after_observed_exit();
                false
            }
            Err(error) => {
                // Losing the ability to supervise the child must fail closed.
                self.exit_diagnostic = Some(bounded_diagnostic(&format!(
                    "managed process supervision failed: {error}"
                )));
                self.force_cleanup();
                false
            }
        }
    }

    /// Take the bounded diagnostic retained after an unexpected child exit or
    /// supervision failure. Expected shutdown paths may ignore this value.
    pub const fn take_exit_diagnostic(&mut self) -> Option<String> {
        self.exit_diagnostic.take()
    }

    /// Gracefully terminate unless `force` requests immediate kill and reap.
    pub fn terminate_interruptibly(&mut self, force: &AtomicBool) {
        self.terminate_with_grace_until(TERMINATION_GRACE_PERIOD, || force.load(Ordering::Acquire));
    }

    /// Immediately kill the owned process group and reap the direct child.
    pub fn force_terminate(&mut self) {
        self.force_cleanup();
    }

    #[cfg(test)]
    fn terminate_with_grace(&mut self, grace_period: Duration) {
        self.terminate_with_grace_until(grace_period, || false);
    }

    fn terminate_with_grace_until(
        &mut self,
        grace_period: Duration,
        should_force: impl Fn() -> bool,
    ) {
        if self.is_cleaned_up() {
            return;
        }

        if should_force() {
            self.force_cleanup();
            return;
        }

        let _ = self.signal_group(libc::SIGTERM);
        if let Some(child) = self.child.as_ref() {
            // This direct-child fallback is harmless when the group signal
            // succeeded and still covers an unexpectedly unavailable group.
            platform::terminate_child(child);
        }

        let deadline = Instant::now() + grace_period;
        while Instant::now() < deadline {
            if should_force() {
                self.force_cleanup();
                return;
            }
            if matches!(self.child.as_ref().map(child_has_exited), Some(Ok(false))) {
                thread::sleep(TERMINATION_POLL_INTERVAL);
            } else {
                // Once the leader exits, no descendant should outlive it.
                // Disarm the still-reserved group before reaping the leader.
                self.force_cleanup();
                return;
            }
        }

        self.force_cleanup();
    }

    const fn is_cleaned_up(&self) -> bool {
        self.child.is_none()
            && self.process_group_id.is_none()
            && self.stdout_reader.is_none()
            && self.stderr_reader.is_none()
    }

    fn signal_group(&self, signal: libc::c_int) -> std::io::Result<()> {
        let Some(process_group_id) = self.process_group_id else {
            return Ok(());
        };
        // SAFETY: the id comes from a child successfully spawned into its own
        // process group. Negating it targets that complete owned group.
        let result = unsafe { libc::kill(-process_group_id, signal) };
        if result == 0 {
            return Ok(());
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn force_cleanup(&mut self) {
        self.force_cleanup_inner(false);
    }

    fn force_cleanup_after_observed_exit(&mut self) {
        self.force_cleanup_inner(true);
    }

    fn force_cleanup_inner(&mut self, record_observed_exit: bool) {
        if self.is_cleaned_up() {
            return;
        }

        // Take/disarm the group before signaling so Drop or another cleanup
        // call can never signal the same (potentially reused) id again.
        if let Some(process_group_id) = self.process_group_id.take() {
            // SAFETY: the id was captured from the managed child at spawn and
            // is consumed here exactly once.
            let _ = unsafe { libc::kill(-process_group_id, libc::SIGKILL) };
        }

        if let Some(mut child) = self.child.take() {
            if let Ok(Some(status)) = child.try_wait() {
                if record_observed_exit {
                    self.exit_diagnostic = Some(bounded_diagnostic(&format!(
                        "managed process exited with {status}; see its service log"
                    )));
                }
            } else {
                let _ = child.kill();
                // The direct child must always be waited after a kill so it
                // cannot remain as a zombie owned by BitEngine.
                let _ = child.wait();
            }
        }

        self.join_readers();
    }

    fn join_readers(&mut self) {
        let deadline = Instant::now() + READER_DRAIN_GRACE_PERIOD;
        while Instant::now() < deadline
            && [self.stdout_reader.as_ref(), self.stderr_reader.as_ref()]
                .into_iter()
                .flatten()
                .any(|reader| !reader.is_finished())
        {
            thread::sleep(TERMINATION_POLL_INTERVAL);
        }

        // A descendant can inherit a pipe and then leave the managed process
        // group. The reader must remain independently cancellable so that such
        // an open writer cannot hang UI reconciliation or application exit.
        self.reader_cancel.store(true, Ordering::Release);
        for reader in [self.stdout_reader.take(), self.stderr_reader.take()]
            .into_iter()
            .flatten()
        {
            let _ = reader.join();
        }
    }
}

fn bounded_diagnostic(message: &str) -> String {
    const MAX_DIAGNOSTIC_CHARS: usize = 512;
    message.chars().take(MAX_DIAGNOSTIC_CHARS).collect()
}

fn child_has_exited(child: &Child) -> std::io::Result<bool> {
    let process_id = libc::id_t::try_from(child.id()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "child process id cannot be represented by waitid",
        )
    })?;
    let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `status` points to writable storage for one `siginfo_t`. WNOWAIT
    // observes an exited owned child without reaping it, keeping its PID and
    // process-group id reserved until `force_cleanup` has disarmed the group.
    loop {
        // SAFETY: see the comment above; retrying after EINTR uses the same
        // still-valid output storage and does not alter child ownership.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                process_id,
                status.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }

    // SAFETY: a successful `waitid` initialized `status`; si_pid is zero when
    // WNOHANG found no waitable state change.
    Ok(unsafe { status.assume_init().si_pid() } != 0)
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // Drop is the fail-safe path for app exit, panic, overwrite, and
        // partially constructed handles. It deliberately skips graceful delay.
        self.force_cleanup();
    }
}

// ── Bitcoin ───────────────────────────────────────────────────────────────────

/// Launch `bitcoind` and stream its output into `queue`.
///
/// Returns a handle to the spawned process and starts background reader threads.
pub fn launch_bitcoind(
    binaries_path: &Path,
    data_dir: &Path,
    rpc_port: u16,
    queue: &OutputQueue,
) -> Result<ProcessHandle> {
    let bitcoind =
        validated_managed_executable(binaries_path, &platform::executable_name("bitcoind"))?;
    let data_dir = platform::prepare_real_directory(data_dir, "Bitcoin data directory", true)?;

    // Command-line settings own the loopback supervision endpoint and cookie
    // authentication. Negating repeatable bind/allow lists restores Core's
    // loopback-only defaults instead of merging less restrictive config values.
    let args = [
        format!("-datadir={}", data_dir.display()),
        "-chain=main".into(),
        // Core resolves these legacy selectors independently of `-chain` and
        // rejects a launch when a true selector is combined with `-chain`.
        // Explicit `=0` values override config; negated CLI forms do not.
        "-testnet=0".into(),
        "-testnet4=0".into(),
        "-signet=0".into(),
        "-regtest=0".into(),
        "-server=1".into(),
        format!("-rpcport={rpc_port}"),
        "-norpcbind".into(),
        "-norpcallowip".into(),
        format!("-rpccookiefile={}", data_dir.join(".cookie").display()),
        "-rpcuser=".into(),
        "-rpcpassword=".into(),
        "-daemon=0".into(),
        "-daemonwait=0".into(),
        "-printtoconsole".into(),
    ];

    push_line(
        queue,
        format!("$ {}", platform::command_display(&bitcoind, &args)),
    );

    let mut command = Command::new(&bitcoind);
    command
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let child = command
        .spawn()
        .with_context(|| format!("spawn bitcoind {}", bitcoind.display()))?;

    spawn_reader_threads(child, queue)
}

// ── Electrs ───────────────────────────────────────────────────────────────────

/// Bitcoin Core connection snapshot passed to one managed Electrs launch.
#[derive(Clone, Copy)]
pub struct ElectrsBitcoinConnection<'a> {
    pub rpc_addr: SocketAddr,
    pub p2p_addr: SocketAddr,
    pub cookie_file: &'a Path,
}

/// Launch `electrs` and stream its output into `queue`.
#[cfg(test)]
pub fn launch_electrs(
    binaries_path: &Path,
    bitcoin_data_dir: &Path,
    electrs_db_dir: &Path,
    electrum_addr: &str,
    connection: ElectrsBitcoinConnection<'_>,
    queue: &OutputQueue,
) -> Result<ProcessHandle> {
    let requested: SocketAddr = electrum_addr
        .parse()
        .context("parse managed electrs listener")?;
    if !requested.ip().is_loopback() {
        bail!(
            "legacy electrs launch accepts only a loopback listener; use the explicit listener API for local-network access"
        );
    }
    let listener =
        ElectrsListenAddr::for_policy(ElectrsBindPolicy::LoopbackOnly, None, requested.port())?;
    launch_electrs_with_listener(
        binaries_path,
        bitcoin_data_dir,
        electrs_db_dir,
        listener,
        connection,
        queue,
    )
}

/// Launch `electrs` with one explicit, validated client-listener snapshot.
///
/// LAN exposure cannot be requested with a wildcard or public address because
/// [`ElectrsListenAddr`] is the only accepted boundary type.
pub fn launch_electrs_with_listener(
    binaries_path: &Path,
    bitcoin_data_dir: &Path,
    electrs_db_dir: &Path,
    listener: ElectrsListenAddr,
    connection: ElectrsBitcoinConnection<'_>,
    queue: &OutputQueue,
) -> Result<ProcessHandle> {
    let electrs = validated_managed_executable(binaries_path, &platform::electrs_binary_name())?;
    let bitcoin_data_dir =
        platform::prepare_real_directory(bitcoin_data_dir, "Bitcoin data directory", false)?;
    let electrs_db_dir =
        platform::prepare_real_directory(electrs_db_dir, "electrs database directory", true)?;
    let args = [
        "--skip-default-conf-files".into(),
        "--network".into(),
        "bitcoin".into(),
        "--daemon-dir".into(),
        bitcoin_data_dir.to_string_lossy().into_owned(),
        "--daemon-rpc-addr".into(),
        connection.rpc_addr.to_string(),
        "--daemon-p2p-addr".into(),
        connection.p2p_addr.to_string(),
        "--cookie-file".into(),
        connection.cookie_file.to_string_lossy().into_owned(),
        "--db-dir".into(),
        electrs_db_dir.to_string_lossy().into_owned(),
        "--electrum-rpc-addr".into(),
        listener.socket_addr().to_string(),
        "--monitoring-addr".into(),
        "127.0.0.1:4224".into(),
    ];

    push_line(
        queue,
        format!("$ {}", platform::command_display(&electrs, &args)),
    );

    let mut command = Command::new(&electrs);
    command
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let child = command
        .spawn()
        .with_context(|| format!("spawn electrs {}", electrs.display()))?;

    spawn_reader_threads(child, queue)
}

fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
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

/// Spawn background threads that read stdout+stderr from `child` into `queue`.
/// Returns a `ProcessHandle` wrapping the child.
///
/// Both stdout and stderr are read concurrently on separate threads that both
/// push into the same queue, preserving approximate interleaving order.
fn spawn_reader_threads(child: Child, queue: &OutputQueue) -> Result<ProcessHandle> {
    let mut handle = ProcessHandle::new(child);
    let child = handle.child.as_mut().context("missing child process")?;
    let stdout = child.stdout.take().context("no stdout pipe")?;
    let stderr = child.stderr.take().context("no stderr pipe")?;
    let process_id = child.id();

    let stdout_queue = Arc::clone(queue);
    let stdout_cancel = Arc::clone(&handle.reader_cancel);
    handle.stdout_reader = Some(
        thread::Builder::new()
            .name(format!("bitengine-node-{process_id}-stdout"))
            .spawn(move || drain_output(stdout, &stdout_queue, &stdout_cancel))
            .context("spawn node stdout reader")?,
    );

    let stderr_queue = Arc::clone(queue);
    let stderr_cancel = Arc::clone(&handle.reader_cancel);
    handle.stderr_reader = Some(
        thread::Builder::new()
            .name(format!("bitengine-node-{process_id}-stderr"))
            .spawn(move || drain_output(stderr, &stderr_queue, &stderr_cancel))
            .context("spawn node stderr reader")?,
    );

    Ok(handle)
}

fn drain_output(
    mut reader: impl std::io::Read + std::os::fd::AsRawFd,
    queue: &OutputQueue,
    cancel: &AtomicBool,
) {
    let mut buffered = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut descriptor = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    while !cancel.load(Ordering::Acquire) {
        descriptor.revents = 0;
        // SAFETY: `descriptor` contains the live descriptor borrowed from
        // `reader`, and the one-element array remains valid for this call.
        let poll_result = unsafe {
            libc::poll(
                std::ptr::addr_of_mut!(descriptor),
                1,
                READER_POLL_INTERVAL_MILLIS,
            )
        };
        if poll_result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if poll_result == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            break;
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }

        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffered.extend_from_slice(&chunk[..read]);
                emit_complete_lines(&mut buffered, queue);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => break,
        }
    }

    if !buffered.is_empty() {
        push_line(queue, String::from_utf8_lossy(&buffered).into_owned());
    }
}

fn emit_complete_lines(buffered: &mut Vec<u8>, queue: &OutputQueue) {
    while let Some(newline) = buffered.iter().position(|byte| *byte == b'\n') {
        emit_output_prefix(buffered, newline, newline.saturating_add(1), queue);
    }
    while buffered.len() > MAX_BUFFERED_OUTPUT_BYTES {
        emit_output_prefix(
            buffered,
            MAX_BUFFERED_OUTPUT_BYTES,
            MAX_BUFFERED_OUTPUT_BYTES,
            queue,
        );
    }
}

fn emit_output_prefix(
    buffered: &mut Vec<u8>,
    line_end: usize,
    consumed: usize,
    queue: &OutputQueue,
) {
    let remainder = buffered.split_off(consumed);
    let mut line = std::mem::replace(buffered, remainder);
    line.truncate(line_end);
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    push_line(queue, String::from_utf8_lossy(&line).into_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::write(path, contents)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(unix)]
    fn spawn_shell(script: &str) -> Result<(ProcessHandle, OutputQueue)> {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let child = command.spawn().context("spawn test shell")?;
        let queue = new_queue();
        let handle = spawn_reader_threads(child, &queue)?;
        Ok((handle, queue))
    }

    #[cfg(unix)]
    fn child_process_id(handle: &ProcessHandle) -> Result<libc::pid_t> {
        handle
            .child
            .as_ref()
            .context("test handle has no child")?
            .id()
            .try_into()
            .context("convert test child process id")
    }

    #[cfg(unix)]
    fn process_exists(process_id: libc::pid_t) -> bool {
        // SAFETY: signal zero only queries the process id supplied by a test
        // child and does not deliver a signal.
        let result = unsafe { libc::kill(process_id, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(unix)]
    fn assert_process_exits(process_id: libc::pid_t) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_exists(process_id) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if process_exists(process_id) {
            bail!("process {process_id} was left running");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn assert_direct_child_reaped(process_id: libc::pid_t) -> Result<()> {
        let mut status = 0;
        // SAFETY: this only checks whether the already-terminated direct test
        // child remains waitable by this process.
        let result =
            unsafe { libc::waitpid(process_id, std::ptr::addr_of_mut!(status), libc::WNOHANG) };
        if result != -1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
            bail!("direct child {process_id} was not reaped");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn wait_for_queue_line(queue: &OutputQueue, prefix: &str) -> Result<String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(lines) = queue.lock() {
                if let Some(line) = lines.iter().find(|line| line.starts_with(prefix)) {
                    return Ok(line.clone());
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
        bail!("timed out waiting for output beginning with {prefix:?}")
    }

    #[cfg(unix)]
    fn wait_for_file(path: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        if !path.exists() {
            bail!("timed out waiting for {}", path.display());
        }
        Ok(())
    }

    #[cfg(unix)]
    fn argument_capture_script(marker: &Path) -> String {
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'READY\\n'\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
            marker.display()
        )
    }

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

        assert!(launch_bitcoind(&binaries, &bitcoin_data, 8332, &queue).is_err());
        assert!(launch_electrs(
            &binaries,
            &bitcoin_data,
            &electrs_data,
            "127.0.0.1:50001",
            ElectrsBitcoinConnection {
                rpc_addr: "127.0.0.1:8332".parse()?,
                p2p_addr: "127.0.0.1:8333".parse()?,
                cookie_file: &bitcoin_data.join(".cookie"),
            },
            &queue
        )
        .is_err());
        assert!(!marker.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dropping_handle_force_kills_and_reaps_child() -> Result<()> {
        let (handle, queue) =
            spawn_shell("printf 'READY\\n'; trap '' TERM; while :; do sleep 1; done")?;
        let process_id = child_process_id(&handle)?;
        let _ = wait_for_queue_line(&queue, "READY")?;

        drop(handle);

        assert_process_exits(process_id)?;
        assert_direct_child_reaped(process_id)
    }

    #[cfg(unix)]
    #[test]
    fn termination_cleans_up_complete_process_group() -> Result<()> {
        let (mut handle, queue) = spawn_shell(
            "sleep 30 & descendant_pid=$!; printf 'DESCENDANT_PID=%s\\n' \"$descendant_pid\"; wait \"$descendant_pid\"",
        )?;
        let parent_id = child_process_id(&handle)?;
        let descendant_line = wait_for_queue_line(&queue, "DESCENDANT_PID=")?;
        let descendant_id = descendant_line
            .trim_start_matches("DESCENDANT_PID=")
            .parse::<libc::pid_t>()
            .context("parse descendant process id")?;

        handle.terminate_with_grace(Duration::from_millis(500));

        assert!(handle.is_cleaned_up());
        assert_process_exits(parent_id)?;
        assert_process_exits(descendant_id)?;
        assert_direct_child_reaped(parent_id)
    }

    #[cfg(unix)]
    #[test]
    fn natural_exit_is_reaped_and_readers_are_joined() -> Result<()> {
        let (mut handle, queue) = spawn_shell("printf 'NATURAL_EXIT\\n'")?;
        let process_id = child_process_id(&handle)?;
        let deadline = Instant::now() + Duration::from_secs(3);

        while handle.is_running() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }

        assert!(!handle.is_running());
        assert!(handle.child.is_none());
        assert!(handle.stdout_reader.is_none());
        assert!(handle.stderr_reader.is_none());
        assert!(queue
            .lock()
            .is_ok_and(|lines| lines.iter().any(|line| line == "NATURAL_EXIT")));
        assert_direct_child_reaped(process_id)
    }

    #[cfg(unix)]
    #[test]
    fn unexpected_exit_retains_a_bounded_actionable_diagnostic() -> Result<()> {
        let (mut handle, _) = spawn_shell("exit 7")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while handle.is_running() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }

        let diagnostic = handle
            .take_exit_diagnostic()
            .context("unexpected exit diagnostic")?;
        assert!(diagnostic.contains('7'), "{diagnostic}");
        assert!(diagnostic.contains("service log"), "{diagnostic}");
        assert!(diagnostic.chars().count() <= 512);
        assert!(handle.take_exit_diagnostic().is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forced_termination_is_bounded_when_sigterm_is_ignored() -> Result<()> {
        let (mut handle, queue) =
            spawn_shell("printf 'READY\\n'; trap '' TERM; while :; do sleep 1; done")?;
        let process_id = child_process_id(&handle)?;
        let _ = wait_for_queue_line(&queue, "READY")?;
        let started = Instant::now();

        handle.terminate_with_grace(Duration::from_millis(100));

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(handle.is_cleaned_up());
        assert_process_exits(process_id)?;
        assert_direct_child_reaped(process_id)
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_cancels_reader_when_an_unrelated_writer_keeps_pipe_open() -> Result<()> {
        use std::{io::Write as _, os::unix::net::UnixStream};

        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("exit 0");
        configure_process_group(&mut command);
        let child = command.spawn().context("spawn short-lived test child")?;
        let mut handle = ProcessHandle::new(child);
        let queue = new_queue();
        let reader_queue = Arc::clone(&queue);
        let reader_cancel = Arc::clone(&handle.reader_cancel);
        let (reader, mut held_writer) = UnixStream::pair()?;
        handle.stdout_reader = Some(thread::spawn(move || {
            drain_output(reader, &reader_queue, &reader_cancel);
        }));
        held_writer.write_all(b"PIPE_HELD_OPEN\n")?;
        let _ = wait_for_queue_line(&queue, "PIPE_HELD_OPEN")?;

        let started = Instant::now();
        while handle.is_running() {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(handle.is_cleaned_up());
        // The writer is deliberately still open here: cleanup completed due
        // to cancellation rather than waiting for pipe EOF.
        drop(held_writer);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn bitcoin_launch_forces_managed_rpc_without_overriding_p2p_exposure() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let binaries = temporary.path().join("Binaries");
        let bitcoin_data = temporary.path().join("BitcoinChain");
        let marker = temporary.path().join("bitcoind-arguments");
        std::fs::create_dir(&binaries)?;
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::write(
            bitcoin_data.join("bitcoin.conf"),
            "chain=test\n\
             testnet=1\n\
             testnet4=1\n\
             signet=1\n\
             regtest=1\n\
             daemon=1\n\
             rpcport=18441\n\
             rpcbind=127.0.0.1:19000\n\
             port=18444\n\
             bind=[::1]:18444\n\
             listen=1\n\
             rpccookiefile=elsewhere.cookie\n\
             rpcuser=legacy-user\n\
             rpcpassword=legacy-password\n",
        )?;
        write_executable(
            &binaries.join("bitcoind"),
            &argument_capture_script(&marker),
        )?;
        let queue = new_queue();

        let handle = launch_bitcoind(&binaries, &bitcoin_data, 18_441, &queue)?;
        wait_for_file(&marker)?;
        let arguments = std::fs::read_to_string(&marker)?;
        let canonical_bitcoin_data = std::fs::canonicalize(&bitcoin_data)?;

        assert!(arguments.lines().any(|argument| argument == "-chain=main"));
        for selector in ["testnet", "testnet4", "signet", "regtest"] {
            assert!(arguments
                .lines()
                .any(|argument| argument == format!("-{selector}=0")));
        }
        assert!(arguments.lines().any(|argument| argument == "-server=1"));
        assert!(arguments
            .lines()
            .any(|argument| argument == "-rpcport=18441"));
        assert!(arguments.lines().any(|argument| argument == "-norpcbind"));
        assert!(arguments
            .lines()
            .any(|argument| argument == "-norpcallowip"));
        assert!(arguments.lines().any(|argument| {
            argument
                == format!(
                    "-rpccookiefile={}",
                    canonical_bitcoin_data.join(".cookie").display()
                )
        }));
        assert!(arguments.lines().any(|argument| argument == "-rpcuser="));
        assert!(arguments
            .lines()
            .any(|argument| argument == "-rpcpassword="));
        assert!(arguments.lines().any(|argument| argument == "-daemon=0"));
        assert!(arguments
            .lines()
            .any(|argument| argument == "-daemonwait=0"));
        for p2p_option in ["-port", "-bind", "-whitebind", "-listen"] {
            assert!(!arguments.lines().any(|argument| {
                argument == p2p_option || argument.starts_with(&format!("{p2p_option}="))
            }));
        }
        drop(handle);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn electrs_launch_uses_the_managed_bitcoin_connection_snapshot() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let binaries = temporary.path().join("Binaries");
        let bitcoin_data = temporary.path().join("BitcoinChain");
        let electrs_data = temporary.path().join("ElectrsDB");
        let marker = temporary.path().join("electrs-arguments");
        std::fs::create_dir(&binaries)?;
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::create_dir(&electrs_data)?;
        std::fs::write(
            bitcoin_data.join("bitcoin.conf"),
            "includeconf=rpc-settings.conf\n",
        )?;
        std::fs::write(
            bitcoin_data.join("rpc-settings.conf"),
            "[main]\nrpcport=18443\n[test]\nrpcport=18332\n",
        )?;
        write_executable(&binaries.join("electrs"), &argument_capture_script(&marker))?;
        let queue = new_queue();
        let cookie_path = std::fs::canonicalize(&bitcoin_data)?.join(".cookie");

        let handle = launch_electrs(
            &binaries,
            &bitcoin_data,
            &electrs_data,
            "127.0.0.1:50001",
            ElectrsBitcoinConnection {
                rpc_addr: "[::1]:18443".parse()?,
                p2p_addr: "[::1]:18444".parse()?,
                cookie_file: &cookie_path,
            },
            &queue,
        )?;
        wait_for_file(&marker)?;
        let arguments = std::fs::read_to_string(&marker)?;
        let arguments = arguments.lines().collect::<Vec<_>>();

        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments == ["--daemon-rpc-addr", "[::1]:18443"] }));
        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments == ["--daemon-p2p-addr", "[::1]:18444"] }));
        assert!(arguments.contains(&"--skip-default-conf-files"));
        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments == ["--monitoring-addr", "127.0.0.1:4224"] }));
        let cookie_file = cookie_path.to_string_lossy().into_owned();
        assert!(arguments
            .windows(2)
            .any(|arguments| { arguments == ["--cookie-file", cookie_file.as_str()] }));
        drop(handle);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn electrs_lan_launch_binds_only_the_validated_private_interface() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let binaries = temporary.path().join("Binaries");
        let bitcoin_data = temporary.path().join("BitcoinChain");
        let electrs_data = temporary.path().join("ElectrsDB");
        let marker = temporary.path().join("electrs-lan-arguments");
        std::fs::create_dir(&binaries)?;
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::create_dir(&electrs_data)?;
        write_executable(&binaries.join("electrs"), &argument_capture_script(&marker))?;
        let queue = new_queue();
        let cookie_path = std::fs::canonicalize(&bitcoin_data)?.join(".cookie");
        let listener = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            Some("192.168.50.8".parse()?),
            50_001,
        )?;

        let handle = launch_electrs_with_listener(
            &binaries,
            &bitcoin_data,
            &electrs_data,
            listener,
            ElectrsBitcoinConnection {
                rpc_addr: "127.0.0.1:8332".parse()?,
                p2p_addr: "127.0.0.1:8333".parse()?,
                cookie_file: &cookie_path,
            },
            &queue,
        )?;
        wait_for_file(&marker)?;
        let arguments = std::fs::read_to_string(&marker)?;
        let arguments = arguments.lines().collect::<Vec<_>>();

        assert!(arguments
            .windows(2)
            .any(|arguments| arguments == ["--electrum-rpc-addr", "192.168.50.8:50001"]));
        assert!(!arguments.contains(&"0.0.0.0:50001"));
        assert!(arguments
            .windows(2)
            .any(|arguments| arguments == ["--monitoring-addr", "127.0.0.1:4224"]));
        drop(handle);
        Ok(())
    }

    #[test]
    fn legacy_electrs_launch_cannot_broaden_the_listener() {
        let queue = new_queue();
        let result = launch_electrs(
            Path::new("/not/consulted"),
            Path::new("/not/consulted"),
            Path::new("/not/consulted"),
            "0.0.0.0:50001",
            ElectrsBitcoinConnection {
                rpc_addr: "127.0.0.1:8332".parse().expect("static RPC address"),
                p2p_addr: "127.0.0.1:8333".parse().expect("static P2P address"),
                cookie_file: Path::new("/not/consulted/.cookie"),
            },
            &queue,
        );

        let error = result.err().expect("wildcard listener must be rejected");
        assert!(error.to_string().contains("only a loopback listener"));
    }
}
