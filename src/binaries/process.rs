//! Cancellable child-process execution with concurrent stdout/stderr draining.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::Instant,
};

use super::{
    environment::{find_in_path, BuildEnvironment},
    BuildEvent, BuildOperationId,
};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(200);
const TERMINATION_GRACE_PERIOD: Duration = Duration::from_secs(2);
const PROCESS_GROUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("build cancelled")]
    Cancelled,
    #[error("could not find executable {program} in the build PATH")]
    NotFound { program: String },
    #[error("could not resolve executable {program}: {source}")]
    Resolve {
        program: String,
        source: std::io::Error,
    },
    #[error("resolved executable for {program} is not a regular executable file: {path:?}")]
    InvalidExecutable { program: String, path: PathBuf },
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
    #[error("durable build output could not be recorded while {program} was running")]
    LogUnavailable { program: String },
}

#[expect(
    clippy::too_many_arguments,
    reason = "the operation identity and independent UI, durable-log, environment, and cancellation inputs are intentionally explicit"
)]
#[cfg(test)]
pub async fn run(
    operation_id: BuildOperationId,
    program: &str,
    arguments: &[String],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
    event_tx: &mpsc::Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), CommandError> {
    run_with_ui_output(
        operation_id,
        program,
        arguments,
        working_directory,
        environment,
        event_tx,
        log_tx,
        cancelled,
        true,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the immutable output presentation choice joins the existing explicit command inputs"
)]
pub async fn run_with_ui_output(
    operation_id: BuildOperationId,
    program: &str,
    arguments: &[String],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
    event_tx: &mpsc::Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    cancelled: &Arc<AtomicBool>,
    verbose_output: bool,
) -> Result<(), CommandError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::Cancelled);
    }

    let executable = resolve_program(program, environment)?;
    let executable_text = executable.to_string_lossy();

    emit_log(
        operation_id,
        event_tx,
        log_tx,
        format!("\n$ {}\n", command_display(&executable_text, arguments)),
    )
    .await;

    let mut command = Command::new(&executable);
    preserve_requested_argv0(&mut command, program);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(&mut command);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }

    let mut child = command.spawn().map_err(|source| CommandError::Spawn {
        program: executable.display().to_string(),
        source,
    })?;
    let mut process_group = ProcessGroupGuard::new(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut stdout_task = stdout.map(|reader| {
        tokio::spawn(drain(
            reader,
            operation_id,
            event_tx.clone(),
            log_tx.clone(),
            "stdout",
            verbose_output,
        ))
    });
    let mut stderr_task = stderr.map(|reader| {
        tokio::spawn(drain(
            reader,
            operation_id,
            event_tx.clone(),
            log_tx.clone(),
            "stderr",
            verbose_output,
        ))
    });

    let mut poll = tokio::time::interval(CANCELLATION_POLL_INTERVAL);
    let status = loop {
        tokio::select! {
            result = child.wait() => {
                match result {
                    Ok(status) => break status,
                    Err(source) => {
                        terminate_child(&mut child, &mut process_group).await;
                        let _ = finish_drain_tasks(&mut stdout_task, &mut stderr_task).await;
                        return Err(CommandError::Wait {
                            program: executable.display().to_string(),
                            source,
                        });
                    }
                }
            }
            _ = poll.tick() => {
                if cancelled.load(Ordering::Acquire) {
                    terminate_child(&mut child, &mut process_group).await;
                    let _ = finish_drain_tasks(&mut stdout_task, &mut stderr_task).await;
                    return Err(CommandError::Cancelled);
                }
            }
        }
    };

    // A well-behaved build tool waits for all of its descendants. If the group
    // still exists after the leader exits, terminate those unexpected
    // background descendants before waiting for their inherited pipes to close.
    terminate_child(&mut child, &mut process_group).await;
    let logs_available = finish_drain_tasks(&mut stdout_task, &mut stderr_task).await;

    if cancelled.load(Ordering::Acquire) {
        return Err(CommandError::Cancelled);
    }

    if !logs_available {
        return Err(CommandError::LogUnavailable {
            program: executable.display().to_string(),
        });
    }

    if status.success() {
        Ok(())
    } else {
        Err(CommandError::Failed {
            program: executable.display().to_string(),
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
    probe_with_limits(
        program,
        arguments,
        working_directory,
        environment,
        PROBE_TIMEOUT,
        MAX_PROBE_OUTPUT_BYTES,
        ProbeOutput::Stdout,
    )
    .await
}

pub async fn probe_stderr(
    program: &str,
    arguments: &[&str],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
) -> Option<String> {
    probe_with_limits(
        program,
        arguments,
        working_directory,
        environment,
        PROBE_TIMEOUT,
        MAX_PROBE_OUTPUT_BYTES,
        ProbeOutput::Stderr,
    )
    .await
}

#[derive(Debug, Clone, Copy)]
enum ProbeOutput {
    Stdout,
    Stderr,
}

async fn probe_with_limits(
    program: &str,
    arguments: &[&str],
    working_directory: Option<&Path>,
    environment: &BuildEnvironment,
    timeout: Duration,
    max_output_bytes: usize,
    output_stream: ProbeOutput,
) -> Option<String> {
    let executable = resolve_program(program, environment).ok()?;
    let mut command = Command::new(executable);
    preserve_requested_argv0(&mut command, program);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .kill_on_drop(true);
    match output_stream {
        ProbeOutput::Stdout => {
            command.stdout(Stdio::piped()).stderr(Stdio::null());
        }
        ProbeOutput::Stderr => {
            command.stdout(Stdio::null()).stderr(Stdio::piped());
        }
    }
    configure_process_group(&mut command);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }

    let mut child = command.spawn().ok()?;
    let mut process_group = ProcessGroupGuard::new(&child);
    let reader: Box<dyn AsyncRead + Unpin + Send> = match output_stream {
        ProbeOutput::Stdout => Box::new(child.stdout.take()?),
        ProbeOutput::Stderr => Box::new(child.stderr.take()?),
    };
    let started = Instant::now();
    let capture_limit = max_output_bytes.saturating_add(1);
    let mut output = Vec::with_capacity(capture_limit.min(8192));
    let mut limited_output = reader.take(u64::try_from(capture_limit).unwrap_or(u64::MAX));

    let read_result = tokio::time::timeout(timeout, limited_output.read_to_end(&mut output)).await;
    if !matches!(read_result, Ok(Ok(_))) || output.len() > max_output_bytes {
        terminate_child(&mut child, &mut process_group).await;
        return None;
    }

    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        terminate_child(&mut child, &mut process_group).await;
        return None;
    }
    let wait_result = tokio::time::timeout(remaining, child.wait()).await;
    let Ok(Ok(status)) = wait_result else {
        terminate_child(&mut child, &mut process_group).await;
        return None;
    };
    terminate_child(&mut child, &mut process_group).await;
    if !status.success() {
        return None;
    }

    String::from_utf8(output)
        .ok()
        .map(|output| output.trim().to_owned())
}

fn resolve_program(program: &str, environment: &BuildEnvironment) -> Result<PathBuf, CommandError> {
    let requested = Path::new(program);
    let candidate = if requested.is_absolute() || requested.components().count() > 1 {
        requested.to_path_buf()
    } else {
        find_in_path(program, environment).ok_or_else(|| CommandError::NotFound {
            program: program.to_owned(),
        })?
    };
    let resolved = fs::canonicalize(&candidate).map_err(|source| CommandError::Resolve {
        program: program.to_owned(),
        source,
    })?;
    let metadata = fs::metadata(&resolved).map_err(|source| CommandError::Resolve {
        program: program.to_owned(),
        source,
    })?;
    if !metadata.file_type().is_file() || !has_executable_permissions(&metadata) {
        return Err(CommandError::InvalidExecutable {
            program: program.to_owned(),
            path: resolved,
        });
    }
    Ok(resolved)
}

#[cfg(unix)]
fn has_executable_permissions(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
const fn has_executable_permissions(_: &fs::Metadata) -> bool {
    true
}

fn configure_process_group(command: &mut Command) {
    #[cfg(unix)]
    command.process_group(0);
}

fn preserve_requested_argv0(command: &mut Command, requested_program: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.as_std_mut().arg0(requested_program);
    }
}

#[derive(Debug)]
struct ProcessGroupGuard {
    #[cfg(unix)]
    process_group_id: Option<i32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(child: &Child) -> Self {
        #[cfg(unix)]
        {
            let process_group_id = child.id().and_then(|id| i32::try_from(id).ok());
            Self {
                armed: process_group_id.is_some(),
                process_group_id,
            }
        }

        #[cfg(not(unix))]
        Self { armed: false }
    }

    #[cfg(unix)]
    fn signal(&self, signal: i32) -> std::io::Result<()> {
        let Some(process_group_id) = self.process_group_id.filter(|_| self.armed) else {
            return Ok(());
        };
        // SAFETY: `process_group_id` comes from a successfully spawned child
        // placed in its own process group. Passing its negation targets that
        // group and does not dereference any pointer or access shared memory.
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

    #[cfg(unix)]
    fn is_alive(&self) -> bool {
        if !self.armed {
            return false;
        }
        let Some(process_group_id) = self.process_group_id else {
            return false;
        };
        // SAFETY: signal zero only queries whether the child-owned process
        // group exists; it does not deliver a signal or access memory.
        let result = unsafe { libc::kill(-process_group_id, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(not(unix))]
    const fn is_alive(&self) -> bool {
        false
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(unix)]
        {
            let _ = self.signal(libc::SIGKILL);
        }
    }
}

async fn terminate_child(child: &mut Child, process_group: &mut ProcessGroupGuard) {
    #[cfg(unix)]
    {
        let _ = process_group.signal(libc::SIGTERM);
        let deadline = Instant::now() + TERMINATION_GRACE_PERIOD;
        while process_group.is_alive() && Instant::now() < deadline {
            let _ = child.try_wait();
            tokio::time::sleep(PROCESS_GROUP_POLL_INTERVAL).await;
        }
        if process_group.is_alive() {
            if process_group.signal(libc::SIGKILL).is_ok() {
                process_group.disarm();
            }
        } else {
            process_group.disarm();
        }
    }

    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
        process_group.disarm();
    }

    let _ = tokio::time::timeout(CHILD_REAP_TIMEOUT, child.wait()).await;
}

async fn finish_drain_tasks(
    stdout_task: &mut Option<JoinHandle<bool>>,
    stderr_task: &mut Option<JoinHandle<bool>>,
) -> bool {
    let (stdout_ok, stderr_ok) = tokio::join!(
        finish_drain_task(stdout_task.take()),
        finish_drain_task(stderr_task.take())
    );
    stdout_ok && stderr_ok
}

async fn finish_drain_task(task: Option<JoinHandle<bool>>) -> bool {
    let Some(mut task) = task else {
        return true;
    };
    match tokio::time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut task).await {
        Ok(Ok(logged)) => logged,
        Ok(Err(_)) => false,
        Err(_) => {
            task.abort();
            let _ = task.await;
            false
        }
    }
}

async fn drain<R>(
    mut reader: R,
    operation_id: BuildOperationId,
    event_tx: mpsc::Sender<BuildEvent>,
    log_tx: mpsc::Sender<String>,
    stream_name: &'static str,
    verbose_output: bool,
) -> bool
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return true,
            Ok(read) => {
                let text = String::from_utf8_lossy(&buffer[..read]);
                if !emit_process_output(
                    operation_id,
                    &event_tx,
                    &log_tx,
                    normalise_output(&text),
                    verbose_output,
                )
                .await
                {
                    return false;
                }
            }
            Err(error) => {
                let _ = emit_log(
                    operation_id,
                    &event_tx,
                    &log_tx,
                    format!("\nCould not read build {stream_name}: {error}\n"),
                )
                .await;
                return false;
            }
        }
    }
}

pub async fn emit_log(
    operation_id: BuildOperationId,
    event_tx: &mpsc::Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    message: String,
) -> bool {
    let _ = event_tx.try_send(BuildEvent::Log {
        operation_id,
        message: message.clone(),
    });
    log_tx.send(message).await.is_ok()
}

async fn emit_process_output(
    operation_id: BuildOperationId,
    event_tx: &mpsc::Sender<BuildEvent>,
    log_tx: &mpsc::Sender<String>,
    message: String,
    verbose_output: bool,
) -> bool {
    if verbose_output {
        let _ = event_tx.try_send(BuildEvent::Log {
            operation_id,
            message: message.clone(),
        });
    }
    log_tx.send(message).await.is_ok()
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

    #[cfg(unix)]
    #[tokio::test]
    async fn concise_output_keeps_raw_process_bytes_in_the_durable_log() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        let helper = temporary.path().join("compiler-fixture");
        fs::write(&helper, "#!/bin/sh\nprintf 'FULL_COMPILER_OUTPUT\\n'\n")?;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755))?;
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), temporary.path().display().to_string());
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (log_tx, mut log_rx) = mpsc::channel(16);
        let cancelled = Arc::new(AtomicBool::new(false));

        run_with_ui_output(
            BuildOperationId(77),
            "compiler-fixture",
            &[],
            None,
            &environment,
            &event_tx,
            &log_tx,
            &cancelled,
            false,
        )
        .await?;
        drop(log_tx);
        drop(event_tx);

        let mut durable = String::new();
        while let Some(message) = log_rx.recv().await {
            durable.push_str(&message);
        }
        let mut presented = String::new();
        while let Some(event) = event_rx.recv().await {
            if let BuildEvent::Log { message, .. } = event {
                presented.push_str(&message);
            }
        }
        assert!(durable.contains("FULL_COMPILER_OUTPUT"));
        assert!(!presented.contains("FULL_COMPILER_OUTPUT"));
        assert!(presented.contains("compiler-fixture"));
        Ok(())
    }

    #[cfg(unix)]
    async fn receive_descendant_pid(receiver: &mut mpsc::Receiver<String>) -> i32 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let message = receiver
                    .recv()
                    .await
                    .expect("helper process should report its descendant PID");
                if let Some(pid) = message.lines().find_map(|line| {
                    line.strip_prefix("DESCENDANT_PID=")
                        .and_then(|value| value.parse::<i32>().ok())
                }) {
                    return pid;
                }
            }
        })
        .await
        .expect("helper process should start promptly")
    }

    #[cfg(unix)]
    fn process_exists(process_id: i32) -> bool {
        // SAFETY: signal zero performs an existence/permission check only and
        // the PID was emitted by the local helper process created by this test.
        let result = unsafe { libc::kill(process_id, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(unix)]
    async fn assert_process_exits(process_id: i32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_exists(process_id) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !process_exists(process_id),
            "helper descendant {process_id} was left running"
        );
    }

    #[cfg(unix)]
    fn descendant_helper_arguments() -> Vec<String> {
        vec![
            "-c".to_owned(),
            "sleep 10 & descendant_pid=$!; printf 'DESCENDANT_PID=%s\\n' \"$descendant_pid\"; wait \"$descendant_pid\"".to_owned(),
        ]
    }

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
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (log_tx, _log_rx) = mpsc::channel(8);
        let cancelled = Arc::new(AtomicBool::new(false));
        let result = run(
            BuildOperationId(1),
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

    #[cfg(unix)]
    #[test]
    fn bare_program_names_resolve_to_canonical_executables() {
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), "/usr/bin:/bin".to_owned());
        let executable = resolve_program("false", &environment)
            .expect("the platform false executable should resolve");
        assert!(executable.is_absolute());
        assert!(executable.is_file());
        assert!(!fs::symlink_metadata(executable)
            .expect("resolved executable metadata")
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    async fn canonical_resolution_preserves_multicall_tool_identity() {
        let environment = super::super::environment::build_environment();
        if super::super::environment::find_in_path("rustc", &environment).is_none() {
            return;
        }
        let output = probe("rustc", &["--version"], None, &environment)
            .await
            .expect("the Rust compiler proxy should run as rustc");
        assert!(
            output.starts_with("rustc "),
            "unexpected proxy output: {output}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_path_entries_are_rejected() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let candidate = temporary.path().join("fake-compiler");
        fs::write(&candidate, b"not executable")?;
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), temporary.path().display().to_string());

        let result = resolve_program("fake-compiler", &environment);

        assert!(matches!(result, Err(CommandError::NotFound { .. })));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_terminates_spawned_descendants() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (log_tx, mut log_rx) = mpsc::channel(16);
        let cancelled = Arc::new(AtomicBool::new(false));
        let task_cancelled = Arc::clone(&cancelled);
        let arguments = descendant_helper_arguments();
        let run_task = tokio::spawn(async move {
            run(
                BuildOperationId(2),
                "/bin/sh",
                &arguments,
                None,
                &environment,
                &event_tx,
                &log_tx,
                &task_cancelled,
            )
            .await
        });
        let descendant_pid = receive_descendant_pid(&mut log_rx).await;

        cancelled.store(true, Ordering::Release);
        let result = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("cancelled command should finish promptly")
            .expect("command task should not panic");

        assert!(matches!(result, Err(CommandError::Cancelled)));
        assert_process_exits(descendant_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_command_future_terminates_spawned_descendants() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (log_tx, mut log_rx) = mpsc::channel(16);
        let cancelled = Arc::new(AtomicBool::new(false));
        let arguments = descendant_helper_arguments();
        let run_task = tokio::spawn(async move {
            run(
                BuildOperationId(3),
                "/bin/sh",
                &arguments,
                None,
                &environment,
                &event_tx,
                &log_tx,
                &cancelled,
            )
            .await
        });
        let descendant_pid = receive_descendant_pid(&mut log_rx).await;

        run_task.abort();
        let _ = run_task.await;

        assert_process_exits(descendant_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_timeout_terminates_the_helper_process() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let started = Instant::now();

        let output = probe_with_limits(
            "/bin/sh",
            &["-c", "sleep 10"],
            None,
            &environment,
            Duration::from_millis(100),
            1024,
            ProbeOutput::Stdout,
        )
        .await;

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_rejects_output_over_the_byte_limit() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let started = Instant::now();

        let output = probe_with_limits(
            "/bin/sh",
            &["-c", "while :; do printf 0123456789abcdef; done"],
            None,
            &environment,
            Duration::from_secs(2),
            128,
            ProbeOutput::Stdout,
        )
        .await;

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_probe_captures_bounded_signature_diagnostics() {
        let environment = std::env::vars().collect::<BuildEnvironment>();
        let output = probe_stderr(
            "/bin/sh",
            &["-c", "printf '[GNUPG:] VALIDSIG fingerprint' >&2"],
            None,
            &environment,
        )
        .await;
        assert_eq!(output.as_deref(), Some("[GNUPG:] VALIDSIG fingerprint"));
    }
}
