//! Persistent, single-job build orchestration.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    os::unix::{
        fs::{OpenOptionsExt as _, PermissionsExt as _},
        io::AsRawFd as _,
    },
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt as _, sync::mpsc};

use super::{
    dependencies,
    environment::{self, BuildEnvironment},
    install::{self, InstallArtifact},
    probe_binary_version, process, BinaryKind, BuildEvent, BuildOperationId, BuildStage,
    ReleaseVersion,
};

const BITCOIN_BUILD_SPACE_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const ELECTRS_BUILD_SPACE_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const MAX_BUILD_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BUILD_STATE_BYTES: u64 = 1024 * 1024;
const RETAINED_JOB_LOGS: usize = 8;
const WORKSPACE_LOCK_NAME: &str = ".bitengine-workspace.lock";
const BUILD_CACHE_NAME: &str = "cache";
const UNCOMMITTED_CACHE_MARKER: &str = ".uncommitted";
const BITCOIN_MANAGED_BINARIES: &[&str] = &[
    "bitcoind",
    "bitcoin-cli",
    "bitcoin",
    "bitcoin-tx",
    "bitcoin-util",
    "bitcoin-wallet",
];
const ELECTRS_MANAGED_BINARIES: &[&str] = &["electrs"];
const ELECTRS_SSH_SIGNER_PRINCIPAL: &str = "me@romanzey.de";
// SSH key published for the electrs maintainer and used by the v0.11.1 tag.
// Fingerprint: SHA256:GifMn7F2swVKyn6MewbQHrYCs4i/bPK7gnwxhuPz/YA.
const ELECTRS_SSH_SIGNER_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAZVq/3fgkildjN/MqEnhrP5550sDpFzGxMwevr5q/9w";
// Bitcoin Core's official v31.1 `contrib/verify-commits/trusted-keys` set.
// Pinning authorization separately from GnuPG owner trust prevents an
// unrelated fully trusted local key from authenticating a redirected source.
const BITCOIN_OPENPGP_SIGNERS: &[&str] = &[
    "E777299FC265DD04793070EB944D35F9AC3DB76A",
    "D1DBF2C4B96F2DEBF4C16654410108112E7EA81F",
    "152812300785C96444D3334D17565732E08E5E41",
    "4D1B3D5ECBA1A7E05371EEBE46800E30FC748A66",
    "A8FC55F3B04BA3146F3492E79303B33A305224CB",
];
// Primary fingerprint of the GPG key published by the electrs maintainer and
// used for the pre-v0.11.1 signed tags.
const ELECTRS_LEGACY_OPENPGP_SIGNERS: &[&str] = &["15C8C3574AE4F1E25F3F35C587CAE5FA46917CBB"];

/// Immutable inputs for one source build and installation.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub operation_id: BuildOperationId,
    pub kind: BinaryKind,
    pub version: ReleaseVersion,
    pub binaries_dir: PathBuf,
    pub workspace: PathBuf,
    pub cores: usize,
    pub keep_source: bool,
    pub clean_build: bool,
    pub verbose_output: bool,
}

/// Successful installation summary returned to the UI.
#[derive(Debug, Clone)]
pub struct BuildSummary {
    pub kind: BinaryKind,
    pub version: ReleaseVersion,
    pub installed: Vec<String>,
    pub log_path: PathBuf,
}

/// A user-safe failure returned by a managed build.
#[derive(Debug, Clone)]
pub struct BuildFailure {
    pub message: String,
    pub cancelled: bool,
    pub conflict: bool,
}

/// Durable status stored in `BitEngine`'s configuration directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistedBuildStatus {
    Running,
    Complete,
    Failed,
    Cancelled,
    Interrupted,
}

/// Durable state for the most recent build. Active jobs are converted to
/// `Interrupted` when `BitEngine` starts again. A separate installation journal
/// then restores an uncommitted old set or finalizes a committed new set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedBuild {
    pub id: String,
    pub kind: BinaryKind,
    pub version: ReleaseVersion,
    pub stage: BuildStage,
    pub status: PersistedBuildStatus,
    pub started_at: u64,
    pub updated_at: u64,
    pub log_path: PathBuf,
    pub error: Option<String>,
    pub installed: Vec<String>,
}

#[derive(Debug, Clone)]
struct ActiveBuild {
    id: String,
    kind: BinaryKind,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct Coordinator {
    active: Mutex<Option<ActiveBuild>>,
}

/// Cloneable backend handle used by the Iced update loop and build task.
#[derive(Debug, Clone)]
pub struct BuildService {
    coordinator: Arc<Coordinator>,
    state_path: Arc<PathBuf>,
    recovered: Arc<Option<PersistedBuild>>,
}

impl BuildService {
    #[must_use]
    pub fn new(state_path: PathBuf) -> Self {
        let recovered = recover_interrupted_job(&state_path);
        Self {
            coordinator: Arc::new(Coordinator::default()),
            state_path: Arc::new(state_path),
            recovered: Arc::new(recovered),
        }
    }

    #[must_use]
    pub fn recovered(&self) -> Option<PersistedBuild> {
        self.recovered.as_ref().clone()
    }

    /// Complete or roll back a durable binary transaction before inventory or
    /// node launch. Callers must block use of the destination if this fails.
    pub fn ensure_installation_recovered(destination: &Path) -> std::result::Result<(), String> {
        recover_destination_installation(destination)
            .map(|_| ())
            .map_err(|error| format!("{error:#}"))
    }

    /// Request safe cancellation of the active child process.
    #[must_use]
    pub fn cancel_current(&self) -> bool {
        self.coordinator
            .active
            .lock()
            .ok()
            .and_then(|active| active.as_ref().map(|build| Arc::clone(&build.cancelled)))
            .is_some_and(|cancelled| {
                cancelled.store(true, Ordering::Release);
                true
            })
    }

    /// Start exactly one build. The coordinator rejects any overlapping
    /// Bitcoin Core/electrs request before it can mutate the workspace.
    #[expect(
        clippy::too_many_lines,
        reason = "the build lifetime keeps preflight, durable status, cleanup, and terminal outcome ordering visible in one place"
    )]
    pub async fn run(
        &self,
        mut request: BuildRequest,
        event_tx: mpsc::Sender<BuildEvent>,
    ) -> Result<BuildSummary, BuildFailure> {
        let (active, _permit) = self.acquire(request.kind)?;
        validate_request_paths(&request).map_err(|error| {
            BuildFailure::failed(format!("Unsafe build path configuration: {error:#}"))
        })?;
        request.workspace = prepare_workspace(&request.workspace).map_err(|error| {
            BuildFailure::failed(format!("Could not prepare the build workspace: {error:#}"))
        })?;
        let _workspace_lock = WorkspaceLock::acquire(&request.workspace).map_err(|error| {
            BuildFailure::failed(format!("Could not reserve the build workspace: {error:#}"))
        })?;
        recover_destination_installation(&request.binaries_dir).map_err(|error| {
            BuildFailure::failed(format!(
                "Binary installation recovery is required before a new build can start: {error:#}"
            ))
        })?;
        cleanup_legacy_sources(&request.workspace).map_err(|error| {
            BuildFailure::failed(format!(
                "Could not safely clean the legacy source cache: {error:#}"
            ))
        })?;

        let jobs_root = prepare_managed_directory(&request.workspace, "jobs").map_err(|error| {
            BuildFailure::failed(format!("Could not prepare build job storage: {error:#}"))
        })?;
        prune_job_directories(&jobs_root).map_err(|error| {
            BuildFailure::failed(format!("Could not safely clean old build jobs: {error:#}"))
        })?;

        let job_dir = jobs_root.join(&active.id);
        let log_path = job_dir.join("build.log");
        if let Err(error) = create_private_directory(&job_dir) {
            return Err(BuildFailure::failed(format!(
                "Could not create the build workspace: {error}"
            )));
        }

        let log_file = match open_build_log(&log_path) {
            Ok(file) => tokio::fs::File::from_std(file),
            Err(error) => {
                return Err(BuildFailure::failed(format!(
                    "Could not create the bounded build log: {error:#}"
                )));
            }
        };
        let (log_tx, log_rx) = mpsc::channel::<String>(256);
        let log_failed = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::clone(&log_failed);
        let log_writer = tokio::spawn(async move {
            let result = write_build_log(log_file, log_rx).await;
            if result.is_err() {
                writer_failed.store(true, Ordering::Release);
            }
            result
        });
        let reporter = Reporter {
            operation_id: request.operation_id,
            event_tx,
            log_tx,
            log_failed,
            state_path: Arc::clone(&self.state_path),
        };
        let now = unix_timestamp();
        let mut persisted = PersistedBuild {
            id: active.id.clone(),
            kind: request.kind,
            version: request.version.clone(),
            stage: BuildStage::CheckingRequirements,
            status: PersistedBuildStatus::Running,
            started_at: now,
            updated_at: now,
            log_path: log_path.clone(),
            error: None,
            installed: Vec::new(),
        };

        let started = reporter
            .stage(&mut persisted, BuildStage::CheckingRequirements)
            .await;
        let pipeline = match started {
            Ok(()) => {
                run_pipeline(
                    &request,
                    &job_dir,
                    &active.id,
                    &active.cancelled,
                    &reporter,
                    &mut persisted,
                )
                .await
            }
            Err(error) => Err(error),
        };

        if pipeline.is_err() {
            if let Err(error) = cleanup_uncommitted_cache(&request).await {
                reporter
                    .log(format!(
                        "Warning: could not clean an uncommitted retained-source entry: {error:#}\n"
                    ))
                    .await;
            }
        } else {
            // Disposable source/work state is useful evidence after a failed
            // build. Remove it only after installation has committed.
            cleanup_current_job(&job_dir, &reporter).await;
        }

        let outcome = match pipeline {
            Ok(installed) => {
                persisted.status = PersistedBuildStatus::Complete;
                persisted.stage = BuildStage::Complete;
                persisted.updated_at = unix_timestamp();
                persisted.installed.clone_from(&installed);
                if let Err(error) = reporter.persist(&persisted).await {
                    reporter
                        .log(format!(
                            "Warning: the successful build state could not be persisted: {error:#}\n"
                        ))
                        .await;
                }
                reporter
                    .log(format!(
                        "\n{} {} is installed and ready.\n",
                        request.kind.label(),
                        request.version
                    ))
                    .await;
                reporter.complete(request.kind).await;
                Ok(BuildSummary {
                    kind: request.kind,
                    version: request.version,
                    installed,
                    log_path,
                })
            }
            Err(error) => {
                // Cancellation is no longer actionable once the persisted
                // stage reaches the installation commit boundary. A queued UI
                // cancel from just before that stage must not relabel an
                // installation/recovery error as an unchanged cancellation.
                let cancelled = active.cancelled.load(Ordering::Acquire)
                    && persisted.stage != BuildStage::Installing;
                persisted.status = if cancelled {
                    PersistedBuildStatus::Cancelled
                } else {
                    PersistedBuildStatus::Failed
                };
                persisted.updated_at = unix_timestamp();
                let message = if cancelled {
                    "Build cancelled. The installed binaries were not changed.".to_owned()
                } else if persisted.stage == BuildStage::Installing {
                    format!("Build failed during installation: {error:#}")
                } else {
                    format!("Build failed: {error:#}. The installed binaries were not changed.")
                };
                persisted.error = Some(message.clone());
                if let Err(error) = reporter.persist(&persisted).await {
                    reporter
                        .log(format!(
                            "Warning: the failed build state could not be persisted: {error:#}\n"
                        ))
                        .await;
                }
                reporter.log(format!("\n{message}\n")).await;
                Err(BuildFailure {
                    message,
                    cancelled,
                    conflict: false,
                })
            }
        };

        drop(reporter);
        if let Ok(Err(error)) = log_writer.await {
            // Installation is already complete or definitively failed at this
            // point, so do not misreport it as rolled back. The next run will
            // expose the retained state and bounded log path.
            eprintln!("BitEngine build log could not be finalized: {error:#}");
        }
        outcome
    }

    fn acquire(&self, kind: BinaryKind) -> Result<(ActiveBuild, ActivePermit), BuildFailure> {
        let mut slot =
            self.coordinator.active.lock().map_err(|_| {
                BuildFailure::failed("Build coordination is unavailable.".to_owned())
            })?;
        if let Some(active) = slot.as_ref() {
            return Err(BuildFailure {
                message: format!(
                    "{} is already building. Wait for it to finish or cancel it first.",
                    active.kind.label()
                ),
                cancelled: false,
                conflict: true,
            });
        }

        let active = ActiveBuild {
            id: format!("{}-{}", unix_timestamp_millis(), kind.slug()),
            kind,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        *slot = Some(active.clone());
        drop(slot);
        let permit = ActivePermit {
            id: active.id.clone(),
            coordinator: Arc::clone(&self.coordinator),
        };
        Ok((active, permit))
    }
}

impl BuildFailure {
    const fn failed(message: String) -> Self {
        Self {
            message,
            cancelled: false,
            conflict: false,
        }
    }
}

#[derive(Debug)]
struct ActivePermit {
    id: String,
    coordinator: Arc<Coordinator>,
}

impl Drop for ActivePermit {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.coordinator.active.lock() {
            if slot.as_ref().is_some_and(|active| active.id == self.id) {
                *slot = None;
            }
        }
    }
}

#[derive(Debug)]
struct WorkspaceLock {
    file: File,
}

#[derive(Debug)]
struct BuildLayout {
    source: PathBuf,
    work: PathBuf,
    cache_root: PathBuf,
    cache_entry: PathBuf,
    uses_cache: bool,
    new_cache: bool,
}

impl WorkspaceLock {
    fn acquire(workspace: &Path) -> Result<Self> {
        let path = workspace.join(WORKSPACE_LOCK_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("open workspace lock {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect workspace lock {}", path.display()))?;
        if !metadata.is_file() {
            bail!("workspace lock is not a regular file: {}", path.display());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure workspace lock {}", path.display()))?;
        // SAFETY: `file` owns a valid descriptor for the lifetime of the lock,
        // and `flock` neither takes ownership nor retains the pointer.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error
                .raw_os_error()
                .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
            {
                bail!(
                    "another BitEngine process is already using build workspace {}",
                    workspace.display()
                );
            }
            return Err(error)
                .with_context(|| format!("lock build workspace {}", workspace.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until `self.file` is dropped
        // immediately after this method returns.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct Reporter {
    operation_id: BuildOperationId,
    event_tx: mpsc::Sender<BuildEvent>,
    log_tx: mpsc::Sender<String>,
    log_failed: Arc<AtomicBool>,
    state_path: Arc<PathBuf>,
}

impl Reporter {
    async fn complete(&self, kind: BinaryKind) {
        let _ = self
            .event_tx
            .send(BuildEvent::Stage {
                operation_id: self.operation_id,
                kind,
                stage: BuildStage::Complete,
            })
            .await;
        let _ = self
            .event_tx
            .send(BuildEvent::Progress {
                operation_id: self.operation_id,
                progress: 1.0,
            })
            .await;
    }

    async fn stage(&self, job: &mut PersistedBuild, next_stage: BuildStage) -> Result<()> {
        self.ensure_log_available()?;
        job.stage = next_stage;
        job.updated_at = unix_timestamp();
        self.persist(job).await?;
        let _ = self
            .event_tx
            .send(BuildEvent::Stage {
                operation_id: self.operation_id,
                kind: job.kind,
                stage: next_stage,
            })
            .await;
        let _ = self
            .event_tx
            .send(BuildEvent::Progress {
                operation_id: self.operation_id,
                progress: next_stage.progress(),
            })
            .await;
        self.log(format!("\n── {} ──\n", next_stage.label())).await;
        Ok(())
    }

    fn progress(&self, progress: f32) {
        let _ = self.event_tx.try_send(BuildEvent::Progress {
            operation_id: self.operation_id,
            progress: progress.clamp(0.0, 1.0),
        });
    }

    async fn log(&self, message: String) {
        if !process::emit_log(self.operation_id, &self.event_tx, &self.log_tx, message).await {
            self.log_failed.store(true, Ordering::Release);
        }
    }

    async fn persist(&self, state: &PersistedBuild) -> Result<()> {
        persist_state_async(&self.state_path, state).await
    }

    fn ensure_log_available(&self) -> Result<()> {
        if self.log_failed.load(Ordering::Acquire) {
            bail!("the durable build log is no longer writable");
        }
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the security-sensitive stage order and final transaction boundary are intentionally linear"
)]
async fn run_pipeline(
    request: &BuildRequest,
    job_dir: &Path,
    job_id: &str,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
    persisted: &mut PersistedBuild,
) -> Result<Vec<String>> {
    let base_environment = check_requirements(request, cancelled, reporter).await?;

    reporter
        .stage(persisted, BuildStage::DownloadingSource)
        .await?;
    let layout = prepare_source(request, job_dir, &base_environment, cancelled, reporter).await?;

    reporter
        .stage(persisted, BuildStage::VerifyingSource)
        .await?;
    let source_commit = match verify_source(
        request.kind,
        &request.version,
        &layout.source,
        &base_environment,
    )
    .await
    {
        Ok(commit) => commit,
        Err(error) if layout.uses_cache && !layout.new_cache => {
            let cache_root = layout.cache_root.clone();
            let cache_entry = layout.cache_entry.clone();
            tokio::task::spawn_blocking(move || remove_confined_entry(&cache_root, &cache_entry))
                .await
                .context("retained source cleanup task stopped unexpectedly")??;
            bail!(
                "retained source failed authentication and was discarded; retry the build to download a fresh copy: {error:#}"
            );
        }
        Err(error) => return Err(error),
    };
    reporter
        .log(format!(
            "Verified signed upstream tag {}, commit {source_commit}, origin, and pristine working tree.\n",
            request.version.tag()
        ))
        .await;
    ensure_not_cancelled(cancelled)?;

    reporter
        .stage(persisted, BuildStage::PreparingBuild)
        .await?;
    prepare_build_directory(&layout, request.clean_build).await?;

    let artifacts = match request.kind {
        BinaryKind::BitcoinCore => {
            build_bitcoin(
                request,
                &layout.source,
                &layout.work,
                &base_environment,
                cancelled,
                reporter,
                persisted,
            )
            .await?
        }
        BinaryKind::Electrs => {
            build_electrs(
                request,
                &layout.source,
                &layout.work,
                &base_environment,
                cancelled,
                reporter,
                persisted,
            )
            .await?
        }
    };

    reporter
        .stage(persisted, BuildStage::VerifyingBinary)
        .await?;
    verify_artifacts(request, &artifacts, reporter).await?;
    ensure_not_cancelled(cancelled)?;

    reporter.stage(persisted, BuildStage::Installing).await?;
    reporter.ensure_log_available()?;
    ensure_not_cancelled(cancelled)?;
    let destination = request.binaries_dir.clone();
    let recovery_destination = destination.clone();
    let install_id = job_id.to_owned();
    let installed_names = artifacts
        .iter()
        .map(|artifact| artifact.name.clone())
        .collect::<Vec<_>>();
    let managed = match request.kind {
        BinaryKind::BitcoinCore => BITCOIN_MANAGED_BINARIES,
        BinaryKind::Electrs => ELECTRS_MANAGED_BINARIES,
    };
    let installation = tokio::task::spawn_blocking(move || {
        install::install_transaction_managed(&artifacts, managed, &destination, &install_id)
    })
    .await
    .context("binary installation task stopped unexpectedly")?;
    let installed = match installation {
        Ok(installed) => installed,
        Err(error) => {
            let recovery = tokio::task::spawn_blocking(move || {
                install::recover_pending_install(&recovery_destination)
            })
            .await
            .context("installation recovery task stopped unexpectedly")?;
            match recovery {
                Ok(install::InstallRecovery::Finalized) => {
                    reporter
                        .log(format!(
                            "Installation commit succeeded; finalized retained transaction metadata after cleanup error: {error:#}\n"
                        ))
                        .await;
                    installed_names
                }
                Ok(install::InstallRecovery::RolledBack) => {
                    bail!(
                        "installation failed and the complete previous binary set was restored: {error:#}"
                    );
                }
                Ok(install::InstallRecovery::None) => {
                    bail!(
                        "installation failed before commit; the existing binary set was not changed: {error:#}"
                    );
                }
                Err(recovery_error) => {
                    bail!(
                        "installation recovery is required before these binaries can be used: {error:#}; automatic recovery failed: {recovery_error:#}"
                    );
                }
            }
        }
    };
    if let Err(error) = finalize_build_cache(request, job_dir, &layout).await {
        reporter
            .log(format!(
                "Warning: the committed installation succeeded, but retained build state could not be finalized: {error:#}\n"
            ))
            .await;
    }
    Ok(installed)
}

async fn verify_artifacts(
    request: &BuildRequest,
    artifacts: &[InstallArtifact],
    reporter: &Reporter,
) -> Result<()> {
    for required in match request.kind {
        BinaryKind::BitcoinCore => &["bitcoind", "bitcoin-cli"][..],
        BinaryKind::Electrs => &["electrs"][..],
    } {
        if !artifacts.iter().any(|artifact| artifact.name == *required) {
            bail!("build produced no required {required} executable");
        }
    }

    for artifact in artifacts {
        let metadata = fs::symlink_metadata(&artifact.source)
            .with_context(|| format!("inspect built artifact {}", artifact.source.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "built artifact is not a regular file: {}",
                artifact.source.display()
            );
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "built artifact is not executable: {}",
                artifact.source.display()
            );
        }
        if artifact.source.file_name().and_then(|name| name.to_str()) != Some(&artifact.name) {
            bail!("built artifact name does not match its source filename");
        }
        let reported_version = probe_binary_version(request.kind, &artifact.source)
            .await?
            .with_context(|| format!("{} did not report a version", artifact.name))?;
        if reported_version != request.version {
            bail!(
                "{} reported version {}, expected {}",
                artifact.name,
                reported_version,
                request.version
            );
        }
        reporter
            .log(format!(
                "Verified {} reports version {}.\n",
                artifact.name, reported_version
            ))
            .await;
    }
    Ok(())
}

async fn check_requirements(
    request: &BuildRequest,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
) -> Result<BuildEnvironment> {
    ensure_supported_platform()?;
    let base_environment = environment::build_environment();
    let dependency_report =
        dependencies::check(request.kind, &request.version, &base_environment).await;
    for dependency in dependency_report
        .items
        .iter()
        .filter(|dependency| dependency.state == dependencies::DependencyState::Ready)
    {
        let version = dependency
            .detected_version
            .as_deref()
            .map_or(String::new(), |version| format!(" {version}"));
        reporter
            .log(format!("Found {}{version}\n", dependency.name))
            .await;
    }
    if !dependency_report.is_ready() {
        bail!(
            "missing build requirements: {}. {}",
            dependency_report.issue_summary(),
            dependency_report.guidance
        );
    }
    ensure_not_cancelled(cancelled)?;

    check_build_space(request.kind, &request.workspace)?;
    Ok(base_environment)
}

#[expect(
    clippy::too_many_lines,
    reason = "source cache validation and fresh-clone activation remain linear for security review"
)]
async fn prepare_source(
    request: &BuildRequest,
    job_dir: &Path,
    environment: &BuildEnvironment,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
) -> Result<BuildLayout> {
    let cache_root = prepare_managed_directory(&request.workspace, BUILD_CACHE_NAME)?;
    let cache_entry = cache_root.join(build_cache_key(request));
    cleanup_uncommitted_cache(request).await?;
    match fs::symlink_metadata(&cache_entry) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "retained build cache is not a regular directory: {}",
                    cache_entry.display()
                );
            }
            let canonical = fs::canonicalize(&cache_entry).with_context(|| {
                format!(
                    "canonicalize retained build cache {}",
                    cache_entry.display()
                )
            })?;
            if canonical.parent() != Some(cache_root.as_path()) {
                bail!(
                    "retained build cache escaped its managed root: {}",
                    cache_entry.display()
                );
            }
            let source = canonical.join("source");
            validate_cached_source_directory(&canonical, &source)?;
            reporter
                .log(format!(
                    "Reusing retained authenticated source for {} {}.\n",
                    request.kind.label(),
                    request.version
                ))
                .await;
            return Ok(BuildLayout {
                source,
                work: canonical.join("work"),
                cache_root,
                cache_entry: canonical,
                uses_cache: true,
                new_cache: false,
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect retained build cache {}", cache_entry.display())
            });
        }
    }

    let (source_root, uses_cache, new_cache) = if request.keep_source {
        create_private_directory(&cache_entry)?;
        create_uncommitted_cache_marker(&cache_entry)?;
        (cache_entry.clone(), true, true)
    } else {
        (job_dir.to_path_buf(), false, false)
    };
    let source = source_root.join("source");
    let partial = source_root.join("source.partial");

    let arguments = vec![
        "clone".to_owned(),
        "--progress".to_owned(),
        "--depth".to_owned(),
        "1".to_owned(),
        "--config".to_owned(),
        "core.hooksPath=/dev/null".to_owned(),
        "--config".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "--branch".to_owned(),
        request.version.tag().to_owned(),
        "--".to_owned(),
        request.kind.repository().to_owned(),
        partial.display().to_string(),
    ];
    let result = process::run_with_ui_output(
        request.operation_id,
        "git",
        &arguments,
        Some(job_dir),
        environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
        request.verbose_output,
    )
    .await;
    if let Err(error) = result {
        let _ = remove_job_entry(&source_root, &partial).await;
        return Err(error.into());
    }

    if let Err(error) = tokio::fs::rename(&partial, &source).await {
        let _ = remove_job_entry(&source_root, &partial).await;
        return Err(error)
            .with_context(|| format!("activate verified source tree {}", source.display()));
    }
    Ok(BuildLayout {
        source,
        work: source_root.join("work"),
        cache_root,
        cache_entry,
        uses_cache,
        new_cache,
    })
}

fn create_uncommitted_cache_marker(cache_entry: &Path) -> Result<()> {
    let marker = cache_entry.join(UNCOMMITTED_CACHE_MARKER);
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&marker)
        .with_context(|| format!("create retained-source marker {}", marker.display()))?;
    file.sync_all()
        .with_context(|| format!("sync retained-source marker {}", marker.display()))?;
    File::open(cache_entry)
        .with_context(|| format!("open retained-source entry {}", cache_entry.display()))?
        .sync_all()
        .with_context(|| format!("sync retained-source entry {}", cache_entry.display()))
}

fn build_cache_key(request: &BuildRequest) -> String {
    format!(
        "{}-{}",
        request.kind.slug(),
        request.version.tag().trim_start_matches('v')
    )
}

fn validate_cached_source_directory(cache_entry: &Path, source: &Path) -> Result<()> {
    if source.parent() != Some(cache_entry) {
        bail!("retained source is outside its cache entry");
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect retained source {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "retained source is not a regular directory: {}",
            source.display()
        );
    }
    let canonical = fs::canonicalize(source)
        .with_context(|| format!("canonicalize retained source {}", source.display()))?;
    if canonical.parent() != Some(cache_entry) {
        bail!(
            "retained source escaped its cache entry: {}",
            source.display()
        );
    }
    Ok(())
}

async fn prepare_build_directory(layout: &BuildLayout, clean_build: bool) -> Result<()> {
    let root = if layout.uses_cache {
        layout.cache_entry.clone()
    } else {
        layout
            .work
            .parent()
            .context("job work directory has no parent")?
            .to_path_buf()
    };
    let work = layout.work.clone();
    tokio::task::spawn_blocking(move || {
        if clean_build {
            remove_confined_entry_if_present(&root, &work)?;
        }
        match fs::symlink_metadata(&work) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!(
                    "build work entry is not a regular directory: {}",
                    work.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&work)?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect build work entry {}", work.display()));
            }
        }
        let canonical = fs::canonicalize(&work)
            .with_context(|| format!("canonicalize build work entry {}", work.display()))?;
        if canonical.parent() != Some(root.as_path()) {
            bail!(
                "build work entry escaped its managed root: {}",
                work.display()
            );
        }
        Ok(())
    })
    .await
    .context("build work preparation task stopped unexpectedly")?
}

async fn finalize_build_cache(
    request: &BuildRequest,
    _job_dir: &Path,
    layout: &BuildLayout,
) -> Result<()> {
    let keep_source = request.keep_source;
    let cache_root = layout.cache_root.clone();
    let cache_entry = layout.cache_entry.clone();
    let uses_cache = layout.uses_cache;
    let new_cache = layout.new_cache;
    tokio::task::spawn_blocking(move || {
        if new_cache {
            if !keep_source {
                bail!("an uncommitted retained-source entry lost its keep-source setting");
            }
            let marker = cache_entry.join(UNCOMMITTED_CACHE_MARKER);
            remove_confined_entry(&cache_entry, &marker)?;
            return File::open(&cache_entry)
                .with_context(|| {
                    format!(
                        "open committed retained-source entry {}",
                        cache_entry.display()
                    )
                })?
                .sync_all()
                .with_context(|| {
                    format!(
                        "sync committed retained-source entry {}",
                        cache_entry.display()
                    )
                });
        }
        if uses_cache {
            if keep_source {
                return Ok(());
            }
            return remove_confined_entry(&cache_root, &cache_entry);
        }
        Ok(())
    })
    .await
    .context("retained build finalization task stopped unexpectedly")?
}

async fn cleanup_uncommitted_cache(request: &BuildRequest) -> Result<()> {
    let cache_root = request.workspace.join(BUILD_CACHE_NAME);
    let cache_entry = cache_root.join(build_cache_key(request));
    tokio::task::spawn_blocking(move || {
        let marker = cache_entry.join(UNCOMMITTED_CACHE_MARKER);
        match fs::symlink_metadata(&marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                remove_confined_entry(&cache_root, &cache_entry)
            }
            Ok(_) => bail!(
                "unsafe uncommitted retained-source marker: {}",
                marker.display()
            ),
            Err(error) => Err(error)
                .with_context(|| format!("inspect retained-source marker {}", marker.display())),
        }
    })
    .await
    .context("uncommitted retained-source cleanup task stopped unexpectedly")?
}

async fn verify_source(
    kind: BinaryKind,
    version: &ReleaseVersion,
    source: &Path,
    environment: &BuildEnvironment,
) -> Result<String> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect source tree {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "source tree is not a regular directory: {}",
            source.display()
        );
    }

    let source_text = source.to_string_lossy();
    let origin = git_probe(&source_text, &["remote", "get-url", "origin"], environment)
        .await
        .context("source has no Git origin")?;
    if origin.trim_end_matches('/') != kind.repository().trim_end_matches('/') {
        bail!("source origin does not match the expected upstream repository");
    }

    let head = git_probe(
        &source_text,
        &["rev-parse", "--verify", "HEAD"],
        environment,
    )
    .await
    .context("source has no checked-out commit")?;
    let tag_ref = format!("refs/tags/{}", version.tag());
    let tag_type = git_probe(&source_text, &["cat-file", "-t", &tag_ref], environment)
        .await
        .context("selected release tag is absent from source")?;
    if tag_type != "tag" {
        bail!("selected release reference is not an annotated signed tag");
    }
    let peeled_tag = format!("{tag_ref}^{{commit}}");
    let tag_commit = git_probe(
        &source_text,
        &["rev-parse", "--verify", &peeled_tag],
        environment,
    )
    .await
    .context("selected release tag does not identify a commit")?;
    if head != tag_commit {
        bail!("checked-out source does not match the selected release tag");
    }

    verify_release_tag(kind, version, source, environment).await?;
    verify_pristine_source(&source_text, environment).await?;
    Ok(head)
}

async fn verify_pristine_source(source: &str, environment: &BuildEnvironment) -> Result<()> {
    let status = git_probe(
        source,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
        environment,
    )
    .await
    .context("could not inspect source working tree")?;
    if !status.is_empty() {
        bail!("source working tree contains modified, ignored, or untracked build inputs");
    }
    Ok(())
}

async fn git_probe(
    source: &str,
    arguments: &[&str],
    environment: &BuildEnvironment,
) -> Option<String> {
    let mut command = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "-C",
        source,
    ];
    command.extend_from_slice(arguments);
    process::probe("git", &command, None, environment).await
}

async fn verify_release_tag(
    kind: BinaryKind,
    version: &ReleaseVersion,
    source: &Path,
    environment: &BuildEnvironment,
) -> Result<()> {
    let source_text = source.to_string_lossy();
    let tag = version.tag();
    let git = environment::find_in_path("git", environment).context("Git is unavailable")?;
    let git_text = git.to_string_lossy();

    if kind == BinaryKind::Electrs && version.is_at_least([0, 11, 1]) {
        if !version.is_exactly([0, 11, 1]) {
            bail!("electrs tag {tag} has no reviewed signer policy in this BitEngine release");
        }
        let verifier = environment::find_in_path("ssh-keygen", environment)
            .context("ssh-keygen is unavailable for electrs tag verification")?;
        let verifier_config = format!("gpg.ssh.program={}", verifier.display());
        let allowed_signers = write_electrs_allowed_signers(
            source
                .parent()
                .context("electrs source has no private job directory")?,
        )?;
        let allowed_config = format!("gpg.ssh.allowedSignersFile={}", allowed_signers.display());
        process::probe(
            &git_text,
            &[
                "-c",
                "gpg.format=ssh",
                "-c",
                "gpg.minTrustLevel=fully",
                "-c",
                &verifier_config,
                "-c",
                &allowed_config,
                "-C",
                &source_text,
                "verify-tag",
                "--",
                tag,
            ],
            None,
            environment,
        )
        .await
        .with_context(|| {
            format!("electrs tag {tag} was not signed by the pinned upstream SSH key")
        })?;
        return Ok(());
    }

    let verifier = ["gpg", "gpg2"]
        .into_iter()
        .find_map(|name| environment::find_in_path(name, environment))
        .with_context(|| format!("GnuPG is unavailable for {} tag verification", kind.label()))?;
    let verifier_config = format!("gpg.openpgp.program={}", verifier.display());
    let verification = process::probe_stderr(
        &git_text,
        &[
            "-c",
            "gpg.format=openpgp",
            "-c",
            "gpg.minTrustLevel=undefined",
            "-c",
            &verifier_config,
            "-C",
            &source_text,
            "verify-tag",
            "--raw",
            "--",
            tag,
        ],
        None,
        environment,
    )
    .await
    .with_context(|| {
        format!(
            "{} tag {tag} does not have a valid OpenPGP signature from an imported key",
            kind.label()
        )
    })?;
    let signature = parse_openpgp_signature(&verification).with_context(|| {
        format!(
            "{} tag {tag} verification did not report exactly one OpenPGP signer",
            kind.label()
        )
    })?;
    let authorized = match kind {
        BinaryKind::BitcoinCore => BITCOIN_OPENPGP_SIGNERS,
        BinaryKind::Electrs => ELECTRS_LEGACY_OPENPGP_SIGNERS,
    };
    if !authorized.contains(&signature.primary) {
        bail!(
            "{} tag {tag} was signed by an unapproved OpenPGP key ({})",
            kind.label(),
            signature.primary
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OpenPgpSignature<'a> {
    signing: &'a str,
    primary: &'a str,
}

fn parse_openpgp_signature(output: &str) -> Option<OpenPgpSignature<'_>> {
    const FAILURE_RECORDS: &[&str] = &[
        "BADSIG",
        "ERRSIG",
        "EXPKEYSIG",
        "EXPSIG",
        "NO_PUBKEY",
        "REVKEYSIG",
    ];
    if output.lines().any(|line| {
        let mut fields = line.split_ascii_whitespace();
        fields.next() == Some("[GNUPG:]")
            && fields
                .next()
                .is_some_and(|record| FAILURE_RECORDS.contains(&record))
    }) {
        return None;
    }

    let mut signatures = output.lines().filter_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("[GNUPG:]") || fields.next() != Some("VALIDSIG") {
            return None;
        }
        let signing = fields.next()?;
        let primary = fields.last()?;
        (valid_openpgp_fingerprint(signing) && valid_openpgp_fingerprint(primary))
            .then_some(OpenPgpSignature { signing, primary })
    });
    let signature = signatures.next()?;
    signatures.next().is_none().then_some(signature)
}

fn valid_openpgp_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 40
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
}

fn write_electrs_allowed_signers(job_dir: &Path) -> Result<PathBuf> {
    let path = job_dir.join("electrs-allowed-signers");
    let contents = format!("{ELECTRS_SSH_SIGNER_PRINCIPAL} {ELECTRS_SSH_SIGNER_KEY}\n");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() != u64::try_from(contents.len()).unwrap_or(u64::MAX)
            {
                bail!("unsafe retained electrs signer data: {}", path.display());
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .with_context(|| format!("open retained electrs signer file {}", path.display()))?;
            let mut retained = String::new();
            file.read_to_string(&mut retained)
                .with_context(|| format!("read retained electrs signer file {}", path.display()))?;
            if retained != contents {
                bail!("retained electrs signer data does not match BitEngine's pinned key");
            }
            return Ok(path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect electrs signer file {}", path.display()));
        }
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("create pinned electrs signer file {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write pinned electrs signer file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync pinned electrs signer file {}", path.display()))?;
    Ok(path)
}

#[allow(clippy::too_many_arguments)]
async fn build_bitcoin(
    request: &BuildRequest,
    source: &Path,
    job_dir: &Path,
    base_environment: &BuildEnvironment,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
    persisted: &mut PersistedBuild,
) -> Result<Vec<InstallArtifact>> {
    let build_dir = job_dir.join("bitcoin");
    let environment = environment::bitcoin_environment(base_environment);
    let mut configure = vec![
        "-S".to_owned(),
        source.display().to_string(),
        "-B".to_owned(),
        build_dir.display().to_string(),
        "-DENABLE_WALLET=OFF".to_owned(),
        "-DENABLE_IPC=OFF".to_owned(),
        "-DBUILD_TESTS=OFF".to_owned(),
        "-DBUILD_BENCH=OFF".to_owned(),
        "-DBUILD_GUI=OFF".to_owned(),
        "-DWITH_ZMQ=OFF".to_owned(),
        "-DCMAKE_FIND_USE_PACKAGE_REGISTRY=FALSE".to_owned(),
        "-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=FALSE".to_owned(),
        "-DCMAKE_EXPORT_NO_PACKAGE_REGISTRY=TRUE".to_owned(),
    ];
    // These optional-network-library switches existed through the v29/v30
    // CMake interface and were removed in v31. Keep old releases node-only
    // without feeding unknown cache entries to newer CMake versions.
    if !request.version.is_at_least([31, 0, 0]) {
        configure.extend([
            "-DWITH_MINIUPNPC=OFF".to_owned(),
            "-DWITH_NATPMP=OFF".to_owned(),
        ]);
    }
    process::run_with_ui_output(
        request.operation_id,
        "cmake",
        &configure,
        None,
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
        request.verbose_output,
    )
    .await
    .context("CMake could not prepare the Bitcoin Core build")?;
    reporter.progress(0.34);

    reporter.stage(persisted, BuildStage::Compiling).await?;
    let build = vec![
        "--build".to_owned(),
        build_dir.display().to_string(),
        "--parallel".to_owned(),
        request.cores.clamp(1, 64).to_string(),
    ];
    process::run_with_ui_output(
        request.operation_id,
        "cmake",
        &build,
        None,
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
        request.verbose_output,
    )
    .await
    .context("Bitcoin Core compilation failed")?;
    reporter.progress(0.82);

    let binary_dir = build_dir.join("bin");
    let mut artifacts = Vec::new();
    for name in BITCOIN_MANAGED_BINARIES {
        let path = binary_dir.join(name);
        if path.is_file() {
            artifacts.push(InstallArtifact {
                source: path,
                name: (*name).to_owned(),
            });
        }
    }
    Ok(artifacts)
}

#[allow(clippy::too_many_arguments)]
async fn build_electrs(
    request: &BuildRequest,
    source: &Path,
    job_dir: &Path,
    base_environment: &BuildEnvironment,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
    persisted: &mut PersistedBuild,
) -> Result<Vec<InstallArtifact>> {
    let target_dir = job_dir.join("electrs-target");
    let environment = environment::cargo_environment(base_environment, &target_dir);
    reporter.stage(persisted, BuildStage::Compiling).await?;
    let arguments = vec![
        "build".to_owned(),
        "--release".to_owned(),
        "--jobs".to_owned(),
        request.cores.clamp(1, 64).to_string(),
        "--locked".to_owned(),
    ];
    process::run_with_ui_output(
        request.operation_id,
        "cargo",
        &arguments,
        Some(source),
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
        request.verbose_output,
    )
    .await
    .context("electrs compilation failed")?;
    reporter.progress(0.82);

    let binary = target_dir.join("release").join("electrs");
    if !binary.is_file() {
        bail!("electrs compilation finished without producing an electrs binary");
    }
    Ok(vec![InstallArtifact {
        source: binary,
        name: "electrs".to_owned(),
    }])
}

fn check_build_space(kind: BinaryKind, workspace: &Path) -> Result<()> {
    let required = match kind {
        BinaryKind::BitcoinCore => BITCOIN_BUILD_SPACE_BYTES,
        BinaryKind::Electrs => ELECTRS_BUILD_SPACE_BYTES,
    };
    if let Some(available) = install::available_space_bytes(workspace) {
        if available < required {
            bail!(
                "insufficient disk space in {} ({} requires approximately {} free; {} available)",
                workspace.display(),
                kind.label(),
                install::format_bytes(required),
                install::format_bytes(available)
            );
        }
    }
    Ok(())
}

fn ensure_supported_platform() -> Result<()> {
    if matches!(
        (std::env::consts::OS, std::env::consts::ARCH),
        ("macos" | "linux", "aarch64") | ("linux", "x86_64")
    ) {
        Ok(())
    } else {
        bail!("source builds support macOS Apple Silicon, Linux x86_64, and Linux ARM64")
    }
}

fn ensure_not_cancelled(cancelled: &Arc<AtomicBool>) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("build cancelled")
    }
    Ok(())
}

fn validate_request_paths(request: &BuildRequest) -> Result<()> {
    validate_absolute_path("binary destination", &request.binaries_dir)?;
    validate_absolute_path("build workspace", &request.workspace)?;
    if request.workspace.starts_with(&request.binaries_dir)
        || request.binaries_dir.starts_with(&request.workspace)
    {
        bail!(
            "binary destination and build workspace must not overlap ({} and {})",
            request.binaries_dir.display(),
            request.workspace.display()
        );
    }
    Ok(())
}

fn validate_absolute_path(label: &str, path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!("{label} must not contain . or ..: {}", path.display());
    }
    if path.parent().is_none() {
        bail!("{label} must not be a filesystem root");
    }
    Ok(())
}

fn prepare_workspace(path: &Path) -> Result<PathBuf> {
    validate_absolute_path("build workspace", path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "build workspace must be a regular directory, not a symbolic link: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .with_context(|| format!("create build workspace {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect build workspace {}", path.display()));
        }
    }
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reinspect build workspace {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "build workspace changed during validation: {}",
            path.display()
        );
    }
    fs::canonicalize(path)
        .with_context(|| format!("canonicalize build workspace {}", path.display()))
}

fn prepare_managed_directory(workspace: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        bail!("invalid managed workspace directory name");
    }
    let path = workspace.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "managed workspace entry is not a regular directory: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(&path)?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspect managed workspace directory {}", path.display())
            });
        }
    }
    let canonical = fs::canonicalize(&path).with_context(|| {
        format!(
            "canonicalize managed workspace directory {}",
            path.display()
        )
    })?;
    if canonical.parent() != Some(workspace) {
        bail!(
            "managed workspace directory escaped its root: {}",
            path.display()
        );
    }
    Ok(canonical)
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| format!("create private directory {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("secure private directory {}", path.display()))?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .with_context(|| format!("open parent directory {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("sync parent directory {}", parent.display()))?;
    }
    Ok(())
}

fn recover_destination_installation(destination: &Path) -> Result<install::InstallRecovery> {
    validate_absolute_path("binary destination", destination)?;
    install::recover_pending_install(destination)
}

fn prune_job_directories(jobs_dir: &Path) -> Result<()> {
    let mut jobs = Vec::new();
    for entry in fs::read_dir(jobs_dir)
        .with_context(|| format!("inspect build jobs {}", jobs_dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_managed_job_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect old build job {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "old build job is not a regular directory: {}",
                path.display()
            );
        }
        for child in [
            "work",
            "source",
            "source.partial",
            "electrs-allowed-signers",
        ] {
            remove_confined_entry_if_present(&path, &path.join(child))?;
        }
        jobs.push((metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), path));
    }
    jobs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in jobs.into_iter().skip(RETAINED_JOB_LOGS.saturating_sub(1)) {
        remove_confined_entry(jobs_dir, &path)?;
    }
    Ok(())
}

fn cleanup_legacy_sources(workspace: &Path) -> Result<()> {
    let sources = workspace.join("sources");
    match fs::symlink_metadata(&sources) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
            "legacy source cache is not a regular directory: {}",
            sources.display()
        ),
        Ok(_) => remove_confined_entry(workspace, &sources),
        Err(error) => {
            Err(error).with_context(|| format!("inspect legacy source cache {}", sources.display()))
        }
    }
}

fn is_managed_job_name(name: &str) -> bool {
    let Some((timestamp, kind)) = name.split_once('-') else {
        return false;
    };
    !timestamp.is_empty()
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(kind, "bitcoin" | "electrs")
}

async fn cleanup_current_job(job_dir: &Path, reporter: &Reporter) {
    for child in [
        "work",
        "source",
        "source.partial",
        "electrs-allowed-signers",
    ] {
        let root = job_dir.to_path_buf();
        let path = root.join(child);
        let result =
            tokio::task::spawn_blocking(move || remove_confined_entry_if_present(&root, &path))
                .await;
        let error = match result {
            Ok(Ok(())) => continue,
            Ok(Err(error)) => format!("{error:#}"),
            Err(error) => error.to_string(),
        };
        reporter
            .log(format!(
                "Warning: could not clean {child} from this build job: {error}\n"
            ))
            .await;
    }
}

async fn remove_job_entry(job_dir: &Path, path: &Path) -> Result<()> {
    let root = job_dir.to_path_buf();
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || remove_confined_entry_if_present(&root, &path))
        .await
        .context("workspace cleanup task stopped unexpectedly")?
}

fn remove_confined_entry_if_present(root: &Path, path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => remove_confined_entry(root, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect workspace entry {}", path.display()))
        }
    }
}

fn remove_confined_entry(root: &Path, path: &Path) -> Result<()> {
    if path.parent() != Some(root) {
        bail!("refusing to remove a workspace entry outside its direct parent");
    }
    let root = fs::canonicalize(root)
        .with_context(|| format!("canonicalize cleanup root {}", root.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect workspace entry {}", path.display()))?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        return fs::remove_file(path)
            .with_context(|| format!("remove workspace file {}", path.display()));
    }
    if !metadata.is_dir() {
        bail!("unsupported workspace entry at {}", path.display());
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize workspace entry {}", path.display()))?;
    if canonical.parent() != Some(root.as_path()) {
        bail!(
            "workspace directory escaped its cleanup root: {}",
            path.display()
        );
    }
    fs::remove_dir_all(&canonical)
        .with_context(|| format!("remove workspace directory {}", canonical.display()))
}

fn open_build_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create build log {}", path.display()))
}

async fn write_build_log(file: tokio::fs::File, receiver: mpsc::Receiver<String>) -> Result<()> {
    write_build_log_with_limit(file, receiver, MAX_BUILD_LOG_BYTES).await
}

async fn write_build_log_with_limit(
    mut file: tokio::fs::File,
    mut receiver: mpsc::Receiver<String>,
    limit: u64,
) -> Result<()> {
    const MARKER: &[u8] =
        b"\n[BitEngine build log reached its 64 MiB limit; further output was discarded.]\n";
    let marker_length = u64::try_from(MARKER.len()).context("build log marker is too large")?;
    let data_limit = limit
        .checked_sub(marker_length)
        .context("build log limit is smaller than its truncation marker")?;
    let mut written = 0_u64;
    let mut capped = false;
    while let Some(message) = receiver.recv().await {
        if capped {
            continue;
        }
        let bytes = message.as_bytes();
        let remaining = data_limit.saturating_sub(written);
        let accepted = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        if accepted > 0 {
            file.write_all(&bytes[..accepted]).await?;
            written = written.saturating_add(accepted as u64);
        }
        if accepted < bytes.len() || written == data_limit {
            file.write_all(MARKER).await?;
            capped = true;
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn persist_state_async(path: &Path, state: &PersistedBuild) -> Result<()> {
    let path = path.to_path_buf();
    let state = state.clone();
    tokio::task::spawn_blocking(move || persist_state(&path, &state))
        .await
        .context("build state persistence task stopped unexpectedly")?
}

fn persist_state(path: &Path, state: &PersistedBuild) -> Result<()> {
    validate_state_id(&state.id)?;
    let parent = path
        .parent()
        .context("build state path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create build state directory {}", parent.display()))?;
    let metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspect build state directory {}", parent.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("build state directory is not a regular directory");
    }
    let json = serde_json::to_vec_pretty(state).context("serialize build state")?;
    let temporary = parent.join(format!(".build-job-{}.tmp", state.id));
    remove_safe_stale_file(&temporary)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("create temporary build state {}", temporary.display()))?;
    file.write_all(&json)
        .with_context(|| format!("write temporary build state {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("sync temporary build state {}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, path)
        .with_context(|| format!("activate build state {}", path.display()))?;
    File::open(parent)
        .with_context(|| format!("open build state directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync build state directory {}", parent.display()))?;
    Ok(())
}

fn validate_state_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid build state identifier");
    }
    Ok(())
}

fn remove_safe_stale_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)
                .with_context(|| format!("remove stale build state {}", path.display()))
        }
        Ok(_) => bail!("unsafe stale build state entry: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect stale build state {}", path.display()))
        }
    }
}

fn recover_interrupted_job(path: &Path) -> Option<PersistedBuild> {
    let bytes = read_bounded_state(path)?;
    let mut state = serde_json::from_slice::<PersistedBuild>(&bytes).ok()?;
    validate_state_id(&state.id).ok()?;
    if state.status == PersistedBuildStatus::Running {
        state.status = PersistedBuildStatus::Interrupted;
        state.updated_at = unix_timestamp();
        state.error = Some(if state.stage == BuildStage::Installing {
            "BitEngine closed during installation. Its durable transaction must be recovered before the binaries are inspected, launched, or replaced."
                .to_owned()
        } else {
            "BitEngine closed before installation began. The existing binaries were left unchanged."
                .to_owned()
        });
        let _ = persist_state(path, &state);
    }
    Some(state)
}

fn read_bounded_state(path: &Path) -> Option<Vec<u8>> {
    let initial = fs::symlink_metadata(path).ok()?;
    if initial.file_type().is_symlink()
        || !initial.is_file()
        || initial.len() > MAX_BUILD_STATE_BYTES
    {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BUILD_STATE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.take(MAX_BUILD_STATE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (u64::try_from(bytes.len()).ok()? <= MAX_BUILD_STATE_BYTES).then_some(bytes)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn unix_timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicting_builds_are_rejected() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let service = BuildService::new(temporary.path().join("state.json"));
        let (_active, _permit) = service
            .acquire(BinaryKind::BitcoinCore)
            .expect("first build should acquire coordinator");
        let failure = service
            .acquire(BinaryKind::Electrs)
            .expect_err("second build should conflict");
        assert!(failure.conflict);
        assert!(failure.message.contains("Bitcoin Core"));
    }

    #[tokio::test]
    async fn clean_build_discards_only_reusable_work_artifacts() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace)?;
        let workspace = fs::canonicalize(workspace)?;
        let cache_root = prepare_managed_directory(&workspace, BUILD_CACHE_NAME)?;
        let cache_entry = cache_root.join("bitcoin-31.1");
        create_private_directory(&cache_entry)?;
        let source = cache_entry.join("source");
        let work = cache_entry.join("work");
        create_private_directory(&source)?;
        create_private_directory(&work)?;
        fs::write(source.join("signed-tag-fixture"), b"authenticated source")?;
        fs::write(work.join("object-file"), b"reusable artifact")?;
        let outside = workspace.join("outside-sentinel");
        fs::write(&outside, b"untouched")?;
        let layout = BuildLayout {
            source: source.clone(),
            work: work.clone(),
            cache_root,
            cache_entry,
            uses_cache: true,
            new_cache: false,
        };

        prepare_build_directory(&layout, true).await?;

        assert_eq!(
            fs::read(source.join("signed-tag-fixture"))?,
            b"authenticated source"
        );
        assert!(work.is_dir());
        assert!(!work.join("object-file").exists());
        assert_eq!(fs::read(outside)?, b"untouched");
        Ok(())
    }

    #[tokio::test]
    async fn keep_source_cache_commits_and_removes_only_inside_managed_entry() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("workspace");
        fs::create_dir(&workspace)?;
        let workspace = fs::canonicalize(workspace)?;
        let cache_root = prepare_managed_directory(&workspace, BUILD_CACHE_NAME)?;
        let job_dir = workspace.join("job");
        create_private_directory(&job_dir)?;
        let cache_entry = cache_root.join("electrs-0.11.1");
        create_private_directory(&cache_entry)?;
        create_uncommitted_cache_marker(&cache_entry)?;
        let source = cache_entry.join("source");
        let work = cache_entry.join("work");
        create_private_directory(&source)?;
        create_private_directory(&work)?;
        fs::write(source.join("authenticated-tag"), b"signed source")?;
        fs::write(work.join("compiled-object"), b"object")?;
        let layout = BuildLayout {
            source,
            work,
            cache_root: cache_root.clone(),
            cache_entry: cache_entry.clone(),
            uses_cache: true,
            new_cache: true,
        };
        let mut request = BuildRequest {
            operation_id: BuildOperationId(88),
            kind: BinaryKind::Electrs,
            version: "v0.11.1".parse().map_err(anyhow::Error::msg)?,
            binaries_dir: temporary.path().join("binaries"),
            workspace,
            cores: 1,
            keep_source: true,
            clean_build: false,
            verbose_output: false,
        };

        finalize_build_cache(&request, &job_dir, &layout).await?;
        assert!(!cache_entry.join(UNCOMMITTED_CACHE_MARKER).exists());
        assert_eq!(
            fs::read(cache_entry.join("source/authenticated-tag"))?,
            b"signed source"
        );
        assert_eq!(
            fs::read(cache_entry.join("work/compiled-object"))?,
            b"object"
        );

        let outside = request.workspace.join("outside-sentinel");
        fs::write(&outside, b"untouched")?;
        request.keep_source = false;
        let cached_layout = BuildLayout {
            source: cache_entry.join("source"),
            work: cache_entry.join("work"),
            cache_root,
            cache_entry: fs::canonicalize(&cache_entry)?,
            uses_cache: true,
            new_cache: false,
        };
        finalize_build_cache(&request, &job_dir, &cached_layout).await?;

        assert!(!cache_entry.exists());
        assert_eq!(fs::read(outside)?, b"untouched");
        Ok(())
    }

    #[test]
    fn running_job_is_recovered_as_interrupted() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("state.json");
        let state = PersistedBuild {
            id: "123-bitcoin".to_owned(),
            kind: BinaryKind::BitcoinCore,
            version: "v30.0".parse().map_err(anyhow::Error::msg)?,
            stage: BuildStage::Compiling,
            status: PersistedBuildStatus::Running,
            started_at: 1,
            updated_at: 1,
            log_path: temporary.path().join("build.log"),
            error: None,
            installed: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec(&state)?)?;

        let service = BuildService::new(path.clone());
        let recovered = service.recovered().context("recovered build")?;
        assert_eq!(recovered.status, PersistedBuildStatus::Interrupted);
        assert!(recovered
            .error
            .as_deref()
            .is_some_and(|error| error.contains("left unchanged")));
        let stored: PersistedBuild = serde_json::from_slice(&fs::read(path)?)?;
        assert_eq!(stored.status, PersistedBuildStatus::Interrupted);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_jobs_directory_is_rejected_without_touching_target() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let workspace = temporary.path().join("workspace");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&workspace)?;
        fs::create_dir_all(&outside)?;
        fs::write(outside.join("victim"), b"preserve")?;
        symlink(&outside, workspace.join("jobs"))?;
        symlink(&outside, workspace.join("sources"))?;

        let workspace = prepare_workspace(&workspace)?;
        assert!(prepare_managed_directory(&workspace, "jobs").is_err());
        assert!(cleanup_legacy_sources(&workspace).is_err());
        assert_eq!(fs::read(outside.join("victim"))?, b"preserve");
        Ok(())
    }

    #[tokio::test]
    async fn durable_build_log_is_bounded() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("build.log");
        let file = tokio::fs::File::from_std(open_build_log(&path)?);
        let (sender, receiver) = mpsc::channel(4);
        let writer = tokio::spawn(write_build_log_with_limit(file, receiver, 256));
        sender.send("x".repeat(1024)).await?;
        sender.send("ignored".repeat(100)).await?;
        drop(sender);
        writer.await??;

        let bytes = fs::read(&path)?;
        assert!(bytes.len() <= 256);
        assert!(String::from_utf8_lossy(&bytes).contains("further output was discarded"));
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[tokio::test]
    async fn ignored_source_inputs_are_not_treated_as_clean() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        fs::create_dir(&source)?;
        let environment = environment::build_environment();
        let git = environment::find_in_path("git", &environment).context("test Git")?;
        let run_git = |arguments: &[&str]| -> Result<()> {
            let status = std::process::Command::new(&git)
                .args(arguments)
                .current_dir(&source)
                .env_clear()
                .envs(&environment)
                .status()?;
            if !status.success() {
                bail!("test Git command failed");
            }
            Ok(())
        };
        run_git(&["init", "--quiet"])?;
        run_git(&["config", "user.email", "fixture@example.invalid"])?;
        run_git(&["config", "user.name", "BitEngine fixture"])?;
        fs::write(source.join(".gitignore"), "*.poison\n")?;
        run_git(&["add", ".gitignore"])?;
        run_git(&[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ])?;
        fs::write(source.join("compiler.poison"), "unexpected build input")?;

        assert!(
            verify_pristine_source(&source.to_string_lossy(), &environment)
                .await
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn openpgp_tag_signer_is_explicitly_authorized() {
        let approved = "[GNUPG:] NEWSIG\n[GNUPG:] VALIDSIG CFB16E21C950F67FA95E558F2EEB9F5CC09526C1 2026-01-01 0 4 0 1 10 00 E777299FC265DD04793070EB944D35F9AC3DB76A\n";
        let signature = parse_openpgp_signature(approved)
            .expect("one valid primary fingerprint should be reported");
        assert_eq!(
            signature.signing,
            "CFB16E21C950F67FA95E558F2EEB9F5CC09526C1"
        );
        assert!(BITCOIN_OPENPGP_SIGNERS.contains(&signature.primary));

        let unrelated = "[GNUPG:] VALIDSIG 0000000000000000000000000000000000000000 2026-01-01 0 4 0 1 10 00 1111111111111111111111111111111111111111";
        let signature = parse_openpgp_signature(unrelated)
            .expect("the unrelated fingerprint is syntactically valid");
        assert!(!BITCOIN_OPENPGP_SIGNERS.contains(&signature.primary));
        assert!(parse_openpgp_signature(&format!("{approved}{unrelated}\n")).is_none());
        assert!(
            parse_openpgp_signature(&format!("[GNUPG:] EXPKEYSIG deadbeef\n{approved}")).is_none()
        );
    }

    #[test]
    fn installing_job_recovery_does_not_claim_binaries_were_unchanged() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("state.json");
        let state = PersistedBuild {
            id: "123-bitcoin".to_owned(),
            kind: BinaryKind::BitcoinCore,
            version: "v31.1".parse().map_err(anyhow::Error::msg)?,
            stage: BuildStage::Installing,
            status: PersistedBuildStatus::Running,
            started_at: 1,
            updated_at: 1,
            log_path: temporary.path().join("build.log"),
            error: None,
            installed: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec(&state)?)?;

        let recovered = recover_interrupted_job(&path).context("recovered state")?;
        let message = recovered.error.context("recovery message")?;
        assert!(message.contains("transaction must be recovered"));
        assert!(!message.contains("left unchanged"));
        Ok(())
    }

    #[test]
    fn malformed_state_identifier_cannot_escape_state_directory() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let state = PersistedBuild {
            id: "../escape".to_owned(),
            kind: BinaryKind::Electrs,
            version: "v0.11.1".parse().map_err(anyhow::Error::msg)?,
            stage: BuildStage::Compiling,
            status: PersistedBuildStatus::Running,
            started_at: 1,
            updated_at: 1,
            log_path: PathBuf::from("build.log"),
            error: None,
            installed: Vec::new(),
        };
        let state_path = temporary.path().join("state.json");
        assert!(persist_state(&state_path, &state).is_err());
        assert!(!state_path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn persisted_state_reader_is_bounded_and_does_not_follow_symlinks() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("target.json");
        let alias = temporary.path().join("build-job.json");
        fs::write(&target, b"{}")?;
        symlink(&target, &alias)?;
        assert!(recover_interrupted_job(&alias).is_none());
        assert_eq!(fs::read(&target)?, b"{}");

        let oversized = temporary.path().join("oversized.json");
        fs::write(
            &oversized,
            vec![b' '; usize::try_from(MAX_BUILD_STATE_BYTES)? + 1],
        )?;
        assert!(recover_interrupted_job(&oversized).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn missing_bitcoin_authenticator_fails_before_touching_existing_binaries() -> Result<()> {
        let environment = environment::build_environment();
        if ["gpg", "gpg2"]
            .into_iter()
            .any(|name| environment::find_in_path(name, &environment).is_some())
        {
            return Ok(());
        }

        let temporary = tempfile::tempdir()?;
        let destination = temporary.path().join("binaries");
        fs::create_dir(&destination)?;
        fs::write(destination.join("bitcoind"), b"working daemon")?;
        fs::write(destination.join("bitcoin-cli"), b"working client")?;
        let service = BuildService::new(temporary.path().join("state.json"));
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let event_drain = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
        let request = BuildRequest {
            operation_id: BuildOperationId(91),
            kind: BinaryKind::BitcoinCore,
            version: "v31.1".parse().map_err(anyhow::Error::msg)?,
            binaries_dir: destination.clone(),
            workspace: temporary.path().join("workspace"),
            cores: 1,
            keep_source: false,
            clean_build: false,
            verbose_output: false,
        };

        let failure = service
            .run(request, event_tx)
            .await
            .expect_err("Bitcoin authentication must fail closed without GnuPG");
        event_drain.await?;
        assert!(failure.message.contains("GnuPG"));
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"working daemon");
        assert_eq!(
            fs::read(destination.join("bitcoin-cli"))?,
            b"working client"
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "downloads, authenticates, and compiles the official electrs release"]
    async fn real_electrs_release_builds_and_installs_transactionally() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let destination = temporary.path().join("binaries");
        let workspace = temporary.path().join("workspace");
        let service = BuildService::new(temporary.path().join("state.json"));
        let (event_tx, mut event_rx) = mpsc::channel(256);
        let event_drain = tokio::spawn(async move { while event_rx.recv().await.is_some() {} });
        let request = BuildRequest {
            operation_id: BuildOperationId(1),
            kind: BinaryKind::Electrs,
            version: "v0.11.1".parse().map_err(anyhow::Error::msg)?,
            binaries_dir: destination.clone(),
            workspace,
            cores: 8,
            keep_source: false,
            clean_build: false,
            verbose_output: false,
        };

        let summary = service
            .run(request, event_tx)
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
        event_drain.await?;

        assert_eq!(summary.installed, vec!["electrs"]);
        let version = probe_binary_version(BinaryKind::Electrs, &destination.join("electrs"))
            .await?
            .context("installed electrs version")?;
        assert_eq!(version.display(), "0.11.1");
        assert!(summary.log_path.metadata()?.len() <= MAX_BUILD_LOG_BYTES);
        Ok(())
    }
}
