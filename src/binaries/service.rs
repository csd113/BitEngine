//! Persistent, single-job build orchestration.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
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
    probe_installed_version, process, BinaryKind, BuildEvent, BuildStage, ReleaseVersion,
};

const BITCOIN_BUILD_SPACE_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const ELECTRS_BUILD_SPACE_BYTES: u64 = 6 * 1024 * 1024 * 1024;

/// Immutable inputs for one source build and installation.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub kind: BinaryKind,
    pub version: ReleaseVersion,
    pub binaries_dir: PathBuf,
    pub workspace: PathBuf,
    pub cores: usize,
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
/// `Interrupted` when `BitEngine` starts again, while installed binaries remain
/// untouched.
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
    pub async fn run(
        &self,
        request: BuildRequest,
        event_tx: Sender<BuildEvent>,
    ) -> Result<BuildSummary, BuildFailure> {
        let (active, _permit) = self.acquire(request.kind)?;
        let job_dir = request.workspace.join("jobs").join(&active.id);
        let log_path = job_dir.join("build.log");

        if let Err(error) = tokio::fs::create_dir_all(&job_dir).await {
            return Err(BuildFailure::failed(format!(
                "Could not create the build workspace: {error}"
            )));
        }

        let (log_tx, log_rx) = mpsc::channel::<String>(256);
        let log_writer = tokio::spawn(write_build_log(log_path.clone(), log_rx));
        let reporter = Reporter {
            event_tx,
            log_tx,
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

        let outcome = match pipeline {
            Ok(installed) => {
                persisted.status = PersistedBuildStatus::Complete;
                persisted.stage = BuildStage::Complete;
                persisted.updated_at = unix_timestamp();
                persisted.installed.clone_from(&installed);
                let _ = reporter.persist(&persisted).await;
                reporter
                    .log(format!(
                        "\n{} {} is installed and ready.\n",
                        request.kind.label(),
                        request.version
                    ))
                    .await;
                let _ = reporter.event_tx.send(BuildEvent::Stage {
                    kind: request.kind,
                    stage: BuildStage::Complete,
                });
                let _ = reporter.event_tx.send(BuildEvent::Progress(1.0));
                Ok(BuildSummary {
                    kind: request.kind,
                    version: request.version,
                    installed,
                    log_path,
                })
            }
            Err(error) => {
                let cancelled = active.cancelled.load(Ordering::Acquire);
                persisted.status = if cancelled {
                    PersistedBuildStatus::Cancelled
                } else {
                    PersistedBuildStatus::Failed
                };
                persisted.updated_at = unix_timestamp();
                let message = if cancelled {
                    "Build cancelled. The installed binaries were not changed.".to_owned()
                } else {
                    format!("Build failed: {error:#}. The installed binaries were not changed.")
                };
                persisted.error = Some(message.clone());
                let _ = reporter.persist(&persisted).await;
                reporter.log(format!("\n{message}\n")).await;
                Err(BuildFailure {
                    message,
                    cancelled,
                    conflict: false,
                })
            }
        };

        drop(reporter);
        let _ = log_writer.await;
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

struct Reporter {
    event_tx: Sender<BuildEvent>,
    log_tx: mpsc::Sender<String>,
    state_path: Arc<PathBuf>,
}

impl Reporter {
    async fn stage(&self, job: &mut PersistedBuild, next_stage: BuildStage) -> Result<()> {
        job.stage = next_stage;
        job.updated_at = unix_timestamp();
        self.persist(job).await?;
        let _ = self.event_tx.send(BuildEvent::Stage {
            kind: job.kind,
            stage: next_stage,
        });
        let _ = self
            .event_tx
            .send(BuildEvent::Progress(next_stage.progress()));
        self.log(format!("\n── {} ──\n", next_stage.label())).await;
        Ok(())
    }

    fn progress(&self, progress: f32) {
        let _ = self
            .event_tx
            .send(BuildEvent::Progress(progress.clamp(0.0, 1.0)));
    }

    async fn log(&self, message: String) {
        process::emit_log(&self.event_tx, &self.log_tx, message).await;
    }

    async fn persist(&self, state: &PersistedBuild) -> Result<()> {
        persist_state_async(&self.state_path, state).await
    }
}

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
    let source = prepare_source(request, job_id, &base_environment, cancelled, reporter).await?;

    reporter
        .stage(persisted, BuildStage::VerifyingSource)
        .await?;
    verify_source(request.kind, &request.version, &source, &base_environment).await?;
    reporter
        .log("Source tag, commit, origin, and clean working tree verified.\n".to_owned())
        .await;
    ensure_not_cancelled(cancelled)?;

    reporter
        .stage(persisted, BuildStage::PreparingBuild)
        .await?;
    let build_dir = job_dir.join("work");
    tokio::fs::create_dir_all(&build_dir)
        .await
        .with_context(|| format!("create job directory {}", build_dir.display()))?;

    let artifacts = match request.kind {
        BinaryKind::BitcoinCore => {
            build_bitcoin(
                request,
                &source,
                &build_dir,
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
                &source,
                &build_dir,
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
    let primary = artifacts
        .iter()
        .find(|artifact| artifact.name == request.kind.primary_binary())
        .with_context(|| {
            format!(
                "build produced no {} executable",
                request.kind.primary_binary()
            )
        })?;
    let reported_version = probe_installed_version(&primary.source)
        .await?
        .context("built binary did not report a version")?;
    if reported_version != request.version {
        bail!(
            "built binary reported version {}, expected {}",
            reported_version,
            request.version
        );
    }
    reporter
        .log(format!(
            "Verified {} reports version {}.\n",
            request.kind.primary_binary(),
            reported_version
        ))
        .await;
    ensure_not_cancelled(cancelled)?;

    reporter.stage(persisted, BuildStage::Installing).await?;
    let destination = request.binaries_dir.clone();
    let install_id = job_id.to_owned();
    let installed = tokio::task::spawn_blocking(move || {
        install::install_transaction(&artifacts, &destination, &install_id)
    })
    .await
    .context("binary installation task stopped unexpectedly")??;
    Ok(installed)
}

async fn check_requirements(
    request: &BuildRequest,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
) -> Result<BuildEnvironment> {
    ensure_supported_platform()?;
    let base_environment = environment::build_environment();
    let dependency_report = dependencies::check(request.kind, &base_environment).await;
    for dependency in &dependency_report.found {
        reporter.log(format!("Found {dependency}\n")).await;
    }
    if !dependency_report.is_ready() {
        bail!(
            "missing build requirements: {}. {}",
            dependency_report.missing.join(", "),
            dependency_report.guidance
        );
    }
    ensure_not_cancelled(cancelled)?;

    tokio::fs::create_dir_all(&request.workspace)
        .await
        .with_context(|| format!("create build workspace {}", request.workspace.display()))?;
    let workspace_metadata = tokio::fs::symlink_metadata(&request.workspace)
        .await
        .with_context(|| format!("inspect build workspace {}", request.workspace.display()))?;
    if workspace_metadata.file_type().is_symlink() || !workspace_metadata.is_dir() {
        bail!(
            "build workspace must be a regular directory, not a symbolic link: {}",
            request.workspace.display()
        );
    }
    check_build_space(request.kind, &request.workspace)?;
    Ok(base_environment)
}

async fn prepare_source(
    request: &BuildRequest,
    job_id: &str,
    environment: &BuildEnvironment,
    cancelled: &Arc<AtomicBool>,
    reporter: &Reporter,
) -> Result<PathBuf> {
    let sources = request.workspace.join("sources");
    tokio::fs::create_dir_all(&sources)
        .await
        .with_context(|| format!("create source cache {}", sources.display()))?;
    cleanup_partial_sources(&sources, request.kind, &request.version).await?;
    let source = sources.join(format!(
        "{}-{}",
        request.kind.slug(),
        request.version.display()
    ));

    if let Ok(metadata) = tokio::fs::symlink_metadata(&source).await {
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && verify_source(request.kind, &request.version, &source, environment)
                .await
                .is_ok()
        {
            reporter
                .log(format!(
                    "Reusing verified source at {}.\n",
                    source.display()
                ))
                .await;
            return Ok(source);
        }
        reporter
            .log("Cached source did not pass verification; downloading a clean copy.\n".to_owned())
            .await;
        remove_workspace_entry(&source).await?;
    }

    let partial = sources.join(format!(
        ".{}-{}-{job_id}.partial",
        request.kind.slug(),
        request.version.display()
    ));
    if tokio::fs::symlink_metadata(&partial).await.is_ok() {
        remove_workspace_entry(&partial).await?;
    }

    let arguments = vec![
        "clone".to_owned(),
        "--progress".to_owned(),
        "--depth".to_owned(),
        "1".to_owned(),
        "--branch".to_owned(),
        request.version.tag().to_owned(),
        "--".to_owned(),
        request.kind.repository().to_owned(),
        partial.display().to_string(),
    ];
    let result = process::run(
        "git",
        &arguments,
        Some(&sources),
        environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
    )
    .await;
    if let Err(error) = result {
        let _ = remove_workspace_entry(&partial).await;
        return Err(error.into());
    }

    if let Err(error) = verify_source(request.kind, &request.version, &partial, environment).await {
        let _ = remove_workspace_entry(&partial).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&partial, &source).await {
        let _ = remove_workspace_entry(&partial).await;
        return Err(error)
            .with_context(|| format!("activate verified source cache {}", source.display()));
    }
    Ok(source)
}

async fn cleanup_partial_sources(
    sources: &Path,
    kind: BinaryKind,
    version: &ReleaseVersion,
) -> Result<()> {
    let prefix = format!(".{}-{}-", kind.slug(), version.display());
    let mut entries = tokio::fs::read_dir(sources)
        .await
        .with_context(|| format!("inspect source cache {}", sources.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".partial") {
            remove_workspace_entry(&entry.path()).await?;
        }
    }
    Ok(())
}

async fn verify_source(
    kind: BinaryKind,
    version: &ReleaseVersion,
    source: &Path,
    environment: &BuildEnvironment,
) -> Result<()> {
    let source_text = source.to_string_lossy();
    let origin = process::probe(
        "git",
        &["-C", &source_text, "remote", "get-url", "origin"],
        None,
        environment,
    )
    .await
    .context("source has no Git origin")?;
    if origin.trim_end_matches('/') != kind.repository().trim_end_matches('/') {
        bail!("source origin does not match the expected upstream repository");
    }

    let head = process::probe(
        "git",
        &["-C", &source_text, "rev-parse", "HEAD"],
        None,
        environment,
    )
    .await
    .context("source has no checked-out commit")?;
    let tag_ref = format!("refs/tags/{}", version.tag());
    let tag_commit = process::probe(
        "git",
        &["-C", &source_text, "rev-list", "-n", "1", &tag_ref],
        None,
        environment,
    )
    .await
    .context("selected release tag is absent from source")?;
    if head != tag_commit {
        bail!("checked-out source does not match the selected release tag");
    }

    let status = process::probe(
        "git",
        &[
            "-C",
            &source_text,
            "status",
            "--porcelain",
            "--untracked-files=no",
        ],
        None,
        environment,
    )
    .await
    .context("could not inspect source working tree")?;
    if !status.is_empty() {
        bail!("source working tree contains uncommitted changes");
    }
    Ok(())
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
    let configure = vec![
        "-S".to_owned(),
        source.display().to_string(),
        "-B".to_owned(),
        build_dir.display().to_string(),
        "-DENABLE_WALLET=OFF".to_owned(),
        "-DENABLE_IPC=OFF".to_owned(),
        "-DBUILD_TESTS=OFF".to_owned(),
        "-DBUILD_BENCH=OFF".to_owned(),
        "-DBUILD_GUI=OFF".to_owned(),
        "-DWITH_MINIUPNPC=OFF".to_owned(),
        "-DWITH_NATPMP=OFF".to_owned(),
        "-DWITH_ZMQ=OFF".to_owned(),
    ];
    process::run(
        "cmake",
        &configure,
        None,
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
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
    process::run(
        "cmake",
        &build,
        None,
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
    )
    .await
    .context("Bitcoin Core compilation failed")?;
    reporter.progress(0.82);

    let binary_dir = build_dir.join("bin");
    let mut artifacts = Vec::new();
    for name in ["bitcoind", "bitcoin-cli", "bitcoin-tx", "bitcoin-util"] {
        let path = binary_dir.join(name);
        if path.is_file() {
            artifacts.push(InstallArtifact {
                source: path,
                name: name.to_owned(),
            });
        }
    }
    if !artifacts.iter().any(|artifact| artifact.name == "bitcoind") {
        bail!("Bitcoin Core compilation finished without producing bitcoind");
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
    process::run(
        "cargo",
        &arguments,
        Some(source),
        &environment,
        &reporter.event_tx,
        &reporter.log_tx,
        cancelled,
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

async fn remove_workspace_entry(path: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .with_context(|| format!("inspect workspace entry {}", path.display()))?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove workspace file {}", path.display()))
    } else if metadata.is_dir() {
        tokio::fs::remove_dir_all(path)
            .await
            .with_context(|| format!("remove workspace directory {}", path.display()))
    } else {
        bail!("unsupported workspace entry at {}", path.display())
    }
}

async fn write_build_log(path: PathBuf, mut receiver: mpsc::Receiver<String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .await
        .with_context(|| format!("open build log {}", path.display()))?;
    while let Some(message) = receiver.recv().await {
        file.write_all(message.as_bytes()).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

async fn persist_state_async(path: &Path, state: &PersistedBuild) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create build state directory {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(state).context("serialize build state")?;
    let temporary = path.with_extension("json.tmp");
    tokio::fs::write(&temporary, json)
        .await
        .with_context(|| format!("write temporary build state {}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("activate build state {}", path.display()))?;
    Ok(())
}

fn recover_interrupted_job(path: &Path) -> Option<PersistedBuild> {
    let bytes = fs::read(path).ok()?;
    let mut state = serde_json::from_slice::<PersistedBuild>(&bytes).ok()?;
    if state.status == PersistedBuildStatus::Running {
        state.status = PersistedBuildStatus::Interrupted;
        state.updated_at = unix_timestamp();
        state.error = Some(
            "BitEngine closed before the build finished. Existing binaries were left unchanged."
                .to_owned(),
        );
        if let Ok(json) = serde_json::to_vec_pretty(&state) {
            let temporary = path.with_extension("json.tmp");
            if fs::write(&temporary, json).is_ok() {
                let _ = fs::rename(temporary, path);
            }
        }
    }
    Some(state)
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

    #[tokio::test]
    async fn stale_partial_downloads_are_removed_before_a_retry() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let sources = temporary.path().join("sources");
        tokio::fs::create_dir_all(sources.join(".bitcoin-30.0-old.partial")).await?;
        tokio::fs::create_dir_all(sources.join("bitcoin-29.0")).await?;
        let version = "v30.0"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;

        cleanup_partial_sources(&sources, BinaryKind::BitcoinCore, &version).await?;

        assert!(!sources.join(".bitcoin-30.0-old.partial").exists());
        assert!(sources.join("bitcoin-29.0").exists());
        Ok(())
    }
}
