//! Iced application — all UI state, messages, view logic, and update handlers.
//!
//! Architecture overview
//! ─────────────────────
//! The app follows the Elm/MVU pattern enforced by Iced 0.14:
//!
//!   * `App`            — immutable snapshot of all UI state.
//!   * `Message`        — every possible event (user action, timer tick,
//!     async task result, process output).
//!   * `App::update()`  — pure function: state + message → new state + `Task`.
//!   * `App::view()`    — pure function: state → `Element<Message>`.
//!   * `App::subscription()` — declares recurring subscriptions (timers).
//!
//! Threading model
//! ───────────────
//! Two OS threads per running process read stdout/stderr and push lines into
//! `Arc<Mutex<VecDeque<String>>>` queues (see `process_manager`).
//!
//! The UI drains those queues on every `OutputTick` (100 ms timer).
//! RPC polling happens on every `RpcTick` (5 s timer) via an async `Task`.
//!
//! This keeps the UI thread non-blocking at all times.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[path = "ui_render.rs"]
mod ui_render;

use iced::{
    time,
    widget::{self, scrollable},
    Element, Subscription, Task,
};
use tokio::sync::mpsc;

use crate::{
    binaries::{
        self, AvailableVersions, BinaryKind, BuildEvent, BuildFailure, BuildOperationId,
        BuildRequest, BuildService, BuildStage, BuildSummary, InstalledVersions, PersistedBuild,
        PersistedBuildStatus, ReleaseVersion,
    },
    bitcoin_config::{resolve_managed_endpoints, ManagedBitcoinEndpoints},
    bitcoin_status,
    config::Config,
    electrs_status::{self, ElectrsStatus},
    platform::{self, Platform},
    process_manager::{self, new_queue, ElectrsBitcoinConnection, OutputQueue, ProcessHandle},
    rpc::{self, BlockchainInfo, NetworkInfo, RpcAuth},
};

const BUILD_EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_BUILD_EVENTS_PER_TICK: usize = 256;

// ── Message ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    // ── Timer ticks ──────────────────────────────────────────────────────────
    /// 100 ms — drain process output queues into terminal buffers.
    OutputTick,
    /// 5 s — poll Bitcoin RPC for chain state.
    RpcTick,

    // ── Path editing ─────────────────────────────────────────────────────────
    BinariesPathChanged(String),
    BitcoinDataPathChanged(String),
    ElectrsDataPathChanged(String),
    BrowseBinaries,
    BrowseBitcoinData,
    BrowseElectrsData,
    BinariesBrowsed(Option<String>),
    BitcoinDataBrowsed(Option<String>),
    ElectrsDataBrowsed(Option<String>),
    SavePaths,
    PathsSaved {
        request_id: u64,
        result: Result<Config, String>,
    },
    TogglePathsPanel,

    // ── Node actions ─────────────────────────────────────────────────────────
    LaunchBitcoin,
    LaunchElectrs,
    ShutdownBoth,
    ShutdownElectrsOnly,

    // ── Navigation and binary builds ─────────────────────────────────────────
    OpenDashboard,
    OpenBinaries,
    RefreshBinaryInfo,
    InstalledVersionsLoaded {
        request_id: u64,
        versions: InstalledVersions,
    },
    AvailableVersionsLoaded {
        request_id: u64,
        versions: AvailableVersions,
    },
    SelectBitcoinVersion(ReleaseVersion),
    SelectElectrsVersion(ReleaseVersion),
    StartBuild(BinaryKind),
    BuildFinished {
        operation_id: BuildOperationId,
        result: Result<BuildSummary, BuildFailure>,
    },
    CancelBuild,
    ToggleBuildDetails,
    ToggleBuildAdvanced,

    // ── Async results ─────────────────────────────────────────────────────────
    StatusPollReceived(StatusPollResult),

    // ── Modal / overlay ───────────────────────────────────────────────────────
    /// Dismiss the info/error overlay.
    DismissOverlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Binaries,
}

#[derive(Debug)]
struct BinaryPageState {
    installed_versions: Option<InstalledVersions>,
    available_versions: Option<AvailableVersions>,
    installed_load: InventoryLoad,
    available_load: InventoryLoad,
    selected_bitcoin: Option<ReleaseVersion>,
    selected_electrs: Option<ReleaseVersion>,
    inventory_request: Option<u64>,
    active_operation: Option<BuildOperationId>,
    displayed_operation: Option<BuildOperationId>,
    active_kind: Option<BinaryKind>,
    displayed_request: Option<BuildRequestPresentation>,
    stage: Option<BuildStage>,
    progress: f32,
    log_lines: Vec<String>,
    disclosures: BinaryDisclosures,
    cancellation_requested: bool,
    error: Option<String>,
    success: Option<String>,
    last_log_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct BuildRequestPresentation {
    operation_id: BuildOperationId,
    kind: BinaryKind,
    version: ReleaseVersion,
    binaries_dir: PathBuf,
    workspace: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InventoryLoad {
    Idle,
    Loading,
}

impl InventoryLoad {
    const fn is_loading(self) -> bool {
        matches!(self, Self::Loading)
    }
}

#[derive(Debug, Default)]
struct BinaryDisclosures {
    build_details: bool,
    advanced: bool,
}

impl BinaryPageState {
    fn new(recovered: Option<PersistedBuild>) -> Self {
        let mut state = Self {
            installed_versions: None,
            available_versions: None,
            installed_load: InventoryLoad::Idle,
            available_load: InventoryLoad::Idle,
            selected_bitcoin: None,
            selected_electrs: None,
            inventory_request: None,
            active_operation: None,
            displayed_operation: None,
            active_kind: None,
            displayed_request: None,
            stage: None,
            progress: 0.0,
            log_lines: Vec::new(),
            disclosures: BinaryDisclosures::default(),
            cancellation_requested: false,
            error: None,
            success: None,
            last_log_path: None,
        };

        if let Some(build) = recovered {
            state.stage = Some(build.stage);
            state.progress = build.stage.progress();
            state.last_log_path = Some(build.log_path);
            match build.status {
                PersistedBuildStatus::Complete => {
                    state.progress = 1.0;
                    state.success = Some(format!(
                        "{} {} was installed successfully.",
                        build.kind.label(),
                        build.version
                    ));
                }
                PersistedBuildStatus::Failed
                | PersistedBuildStatus::Cancelled
                | PersistedBuildStatus::Interrupted => {
                    state.error = build.error;
                }
                PersistedBuildStatus::Running => {
                    state.error = Some(if build.stage == BuildStage::Installing {
                        "The previous build stopped during installation. Binary transaction recovery must finish before the destination is used."
                            .to_owned()
                    } else {
                        "The previous build stopped before installation. Existing binaries were left unchanged."
                            .to_owned()
                    });
                }
            }
        }
        state
    }

    const fn can_cancel(&self) -> bool {
        self.active_operation.is_some()
            && !self.cancellation_requested
            && !matches!(
                self.stage,
                Some(BuildStage::Installing | BuildStage::Complete)
            )
    }
}

// ── App state ─────────────────────────────────────────────────────────────────

/// Owns a blocking node-shutdown worker until its process has been reaped.
///
/// Dropping the worker ends any extended graceful wait and joins the thread, so
/// application exit cannot abandon a child inside a detached task.
struct ShutdownWorker {
    force: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ShutdownWorker {
    fn spawn(
        name: &str,
        work: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> std::io::Result<Self> {
        let force = Arc::new(AtomicBool::new(false));
        let worker_force = Arc::clone(&force);
        let thread = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || work(worker_force))?;
        Ok(Self {
            force,
            thread: Some(thread),
        })
    }

    fn request_force(&self) {
        self.force.store(true, Ordering::Release);
    }

    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join_if_finished(&mut self) -> Option<thread::Result<()>> {
        if !self.is_finished() {
            return None;
        }
        self.thread.take().map(JoinHandle::join)
    }
}

impl Drop for ShutdownWorker {
    fn drop(&mut self) {
        self.request_force();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusPollIdentity {
    request_id: u64,
    lifecycle_generation: u64,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "UI process, synchronization, and independent RPC/P2P readiness facts must remain separately observable"
)]
pub struct App {
    // ── Config ───────────────────────────────────────────────────────────────
    config: Config,

    // ── Editable path fields (may differ from saved config until Save Paths) ──
    binaries_path_edit: String,
    bitcoin_data_path_edit: String,
    electrs_data_path_edit: String,

    // ── Process handles ───────────────────────────────────────────────────────
    bitcoin_handle: Option<ProcessHandle>,
    electrs_handle: Option<ProcessHandle>,
    bitcoin_shutdown: Option<ShutdownWorker>,
    electrs_shutdown: Option<ShutdownWorker>,
    managed_endpoints: Option<ManagedBitcoinEndpoints>,
    active_rpc_addr: Option<SocketAddr>,
    active_p2p_addr: Option<SocketAddr>,

    // ── Output queues (filled by background threads, drained by OutputTick) ──
    bitcoin_queue: OutputQueue,
    electrs_queue: OutputQueue,

    // ── Terminal display buffers ───────────────────────────────────────────────
    bitcoin_lines: Vec<String>,
    electrs_lines: Vec<String>,

    // ── Node status ───────────────────────────────────────────────────────────
    bitcoin_running: bool,
    bitcoin_synced: bool,
    bitcoin_rpc_reachable: bool,
    bitcoin_p2p_reachable: bool,
    bitcoin_rpc_error: Option<String>,
    bitcoin_p2p_error: Option<String>,
    bitcoin_compatibility_error: Option<String>,
    electrs_status: ElectrsStatus,
    block_height: u64,

    // ── UI state ──────────────────────────────────────────────────────────────
    page: Page,
    paths_visible: bool,
    binary_page: BinaryPageState,
    build_service: BuildService,
    build_event_tx: mpsc::Sender<BuildEvent>,
    build_event_rx: mpsc::Receiver<BuildEvent>,
    pending_path_save: Option<u64>,
    next_path_save: u64,
    next_inventory_request: u64,
    next_build_operation: u64,
    lifecycle_generation: u64,
    next_status_poll: u64,
    active_status_poll: Option<StatusPollIdentity>,

    /// Non-empty ⇒ display an overlay dialog with this message.
    overlay_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StatusPollResult {
    identity: StatusPollIdentity,
    bitcoin_probe: BitcoinProbeResult,
    electrs_status: ElectrsStatus,
}

#[derive(Debug, Clone)]
struct BitcoinProbeResult {
    blockchain_info: Result<BlockchainInfo, String>,
    network_info: Result<NetworkInfo, String>,
    rpc_addr: Option<SocketAddr>,
    p2p_result: Result<SocketAddr, String>,
}

impl App {
    pub fn new(ssd_root: &Path) -> Self {
        let (config, config_warning) = Config::load(ssd_root);
        Self::from_config(
            config,
            config_warning.as_deref(),
            Config::build_state_file_path(),
        )
    }

    fn from_config(
        config: Config,
        config_warning: Option<&str>,
        build_state_path: PathBuf,
    ) -> Self {
        let binaries_edit = config.binaries_path.to_string_lossy().into_owned();
        let bitcoin_data_edit = config.bitcoin_data_path.to_string_lossy().into_owned();
        let electrs_data_edit = config.electrs_data_path.to_string_lossy().into_owned();

        let bitcoin_queue = new_queue();
        let electrs_queue = new_queue();
        let (build_event_tx, build_event_rx) = mpsc::channel(BUILD_EVENT_QUEUE_CAPACITY);
        let installation_recovery_error =
            BuildService::ensure_installation_recovered(&config.binaries_path).err();
        let build_service = BuildService::new(build_state_path);
        let mut binary_page = BinaryPageState::new(build_service.recovered());
        if let Some(error) = installation_recovery_error.as_ref() {
            binary_page.error = Some(format!(
                "Binary installation recovery is required before launch or update: {error}"
            ));
        }

        // Log startup info into the terminal queues
        push_msg(&bitcoin_queue, "=== BitEngine started ===");
        push_msg(
            &bitcoin_queue,
            &format!("Platform : {}", Platform::current().label()),
        );
        if let Some(warning) = config_warning {
            push_msg(&bitcoin_queue, warning);
            push_msg(&electrs_queue, warning);
        }
        push_msg(
            &bitcoin_queue,
            &format!("Config   : {}", Config::config_file_path().display()),
        );
        push_msg(
            &bitcoin_queue,
            &format!("Binaries : {}", config.binaries_path.display()),
        );
        log_binary_resolution(&bitcoin_queue, &config.binaries_path, "bitcoind");
        push_msg(
            &bitcoin_queue,
            &format!("Data dir : {}", config.bitcoin_data_path.display()),
        );
        push_msg(&electrs_queue, "=== BitEngine started ===");
        push_msg(
            &electrs_queue,
            &format!("Platform : {}", Platform::current().label()),
        );
        push_msg(
            &electrs_queue,
            &format!("Binaries : {}", config.binaries_path.display()),
        );
        log_binary_resolution(&electrs_queue, &config.binaries_path, "electrs");
        push_msg(
            &electrs_queue,
            &format!("DB dir   : {}", config.electrs_data_path.display()),
        );

        Self {
            config,
            binaries_path_edit: binaries_edit,
            bitcoin_data_path_edit: bitcoin_data_edit,
            electrs_data_path_edit: electrs_data_edit,
            bitcoin_handle: None,
            electrs_handle: None,
            bitcoin_shutdown: None,
            electrs_shutdown: None,
            managed_endpoints: None,
            active_rpc_addr: None,
            active_p2p_addr: None,
            bitcoin_queue,
            electrs_queue,
            bitcoin_lines: Vec::new(),
            electrs_lines: Vec::new(),
            bitcoin_running: false,
            bitcoin_synced: false,
            bitcoin_rpc_reachable: false,
            bitcoin_p2p_reachable: false,
            bitcoin_rpc_error: None,
            bitcoin_p2p_error: None,
            bitcoin_compatibility_error: None,
            electrs_status: ElectrsStatus::default(),
            block_height: 0,
            page: Page::Dashboard,
            paths_visible: true,
            binary_page,
            build_service,
            build_event_tx,
            build_event_rx,
            pending_path_save: None,
            next_path_save: 1,
            next_inventory_request: 1,
            next_build_operation: 1,
            lifecycle_generation: 1,
            next_status_poll: 1,
            active_status_poll: None,
            overlay_message: installation_recovery_error.map(|error| {
                format!(
                    "BitEngine could not safely recover an interrupted binary installation. Node launch and binary inventory are blocked until this is resolved:\n\n{error}"
                )
            }),
        }
    }

    // ── update ────────────────────────────────────────────────────────────────

    #[expect(
        clippy::too_many_lines,
        reason = "the exhaustive Iced message dispatcher keeps UI state transitions centralized"
    )]
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OutputTick => self.handle_output_tick(),
            Message::RpcTick => self.handle_rpc_tick(),
            Message::BinariesPathChanged(s) => {
                if self.paths_are_editable() {
                    self.binaries_path_edit = s;
                }
                Task::none()
            }
            Message::BitcoinDataPathChanged(s) => {
                if self.paths_are_editable() {
                    self.bitcoin_data_path_edit = s;
                }
                Task::none()
            }
            Message::ElectrsDataPathChanged(s) => {
                if self.paths_are_editable() {
                    self.electrs_data_path_edit = s;
                }
                Task::none()
            }
            Message::BrowseBinaries => {
                if self.paths_are_editable() {
                    Task::perform(
                        async { browse_folder("Select Binaries Folder").await },
                        Message::BinariesBrowsed,
                    )
                } else {
                    Task::none()
                }
            }
            Message::BrowseBitcoinData => {
                if self.paths_are_editable() {
                    Task::perform(
                        async { browse_folder("Select Bitcoin Data Directory").await },
                        Message::BitcoinDataBrowsed,
                    )
                } else {
                    Task::none()
                }
            }
            Message::BrowseElectrsData => {
                if self.paths_are_editable() {
                    Task::perform(
                        async { browse_folder("Select Electrs DB Directory").await },
                        Message::ElectrsDataBrowsed,
                    )
                } else {
                    Task::none()
                }
            }
            Message::BinariesBrowsed(p) => {
                if let Some(s) = p.filter(|_| self.paths_are_editable()) {
                    self.binaries_path_edit = s;
                }
                Task::none()
            }
            Message::BitcoinDataBrowsed(p) => {
                if let Some(s) = p.filter(|_| self.paths_are_editable()) {
                    self.bitcoin_data_path_edit = s;
                }
                Task::none()
            }
            Message::ElectrsDataBrowsed(p) => {
                if let Some(s) = p.filter(|_| self.paths_are_editable()) {
                    self.electrs_data_path_edit = s;
                }
                Task::none()
            }
            Message::SavePaths => self.save_paths(),
            Message::PathsSaved { request_id, result } => {
                self.apply_paths_saved(request_id, result)
            }
            Message::TogglePathsPanel => {
                self.paths_visible = !self.paths_visible;
                Task::none()
            }
            Message::LaunchBitcoin => self.launch_bitcoin(),
            Message::LaunchElectrs => self.launch_electrs(),
            Message::ShutdownBoth => self.shutdown_both(),
            Message::ShutdownElectrsOnly => self.shutdown_electrs_only(),
            Message::OpenDashboard => {
                self.page = Page::Dashboard;
                Task::none()
            }
            Message::OpenBinaries => {
                self.page = Page::Binaries;
                self.refresh_binary_info()
            }
            Message::RefreshBinaryInfo => self.refresh_binary_info(),
            Message::InstalledVersionsLoaded {
                request_id,
                versions,
            } => {
                if self.binary_page.inventory_request == Some(request_id) {
                    self.binary_page.installed_load = InventoryLoad::Idle;
                    self.binary_page.installed_versions = Some(versions);
                }
                Task::none()
            }
            Message::AvailableVersionsLoaded {
                request_id,
                versions,
            } => {
                if self.binary_page.inventory_request == Some(request_id) {
                    self.apply_available_versions(versions);
                }
                Task::none()
            }
            Message::SelectBitcoinVersion(version) => {
                self.binary_page.selected_bitcoin = Some(version);
                Task::none()
            }
            Message::SelectElectrsVersion(version) => {
                self.binary_page.selected_electrs = Some(version);
                Task::none()
            }
            Message::StartBuild(kind) => self.start_build(kind),
            Message::BuildFinished {
                operation_id,
                result,
            } => self.apply_build_finished(operation_id, result),
            Message::CancelBuild => {
                if self.binary_page.can_cancel() && self.build_service.cancel_current() {
                    self.binary_page.cancellation_requested = true;
                }
                Task::none()
            }
            Message::ToggleBuildDetails => {
                self.binary_page.disclosures.build_details =
                    !self.binary_page.disclosures.build_details;
                Task::none()
            }
            Message::ToggleBuildAdvanced => {
                self.binary_page.disclosures.advanced = !self.binary_page.disclosures.advanced;
                Task::none()
            }
            Message::StatusPollReceived(result) => {
                self.apply_status_poll(result);
                Task::none()
            }
            Message::DismissOverlay => {
                self.overlay_message = None;
                Task::none()
            }
        }
    }

    fn handle_output_tick(&mut self) -> Task<Message> {
        const MAX: usize = 5_000;
        let mut btc_new = false;
        let mut els_new = false;
        let mut build_new = false;

        if let Ok(mut q) = self.bitcoin_queue.lock() {
            while let Some(line) = q.pop_front() {
                self.bitcoin_lines.push(line);
                btc_new = true;
            }
        }

        if let Ok(mut q) = self.electrs_queue.lock() {
            while let Some(line) = q.pop_front() {
                self.electrs_lines.push(line);
                els_new = true;
            }
        }

        let mut build_events_drained = 0;
        while build_events_drained < MAX_BUILD_EVENTS_PER_TICK {
            let Ok(event) = self.build_event_rx.try_recv() else {
                break;
            };
            build_events_drained += 1;
            build_new |= matches!(event, BuildEvent::Log { .. });
            self.apply_build_event(event);
        }
        let build_events_pending = !self.build_event_rx.is_empty();

        if self.bitcoin_lines.len() > MAX {
            let drain_to = self.bitcoin_lines.len() - MAX;
            self.bitcoin_lines.drain(..drain_to);
        }
        if self.electrs_lines.len() > MAX {
            let drain_to = self.electrs_lines.len() - MAX;
            self.electrs_lines.drain(..drain_to);
        }
        if self.binary_page.log_lines.len() > MAX {
            let drain_to = self.binary_page.log_lines.len() - MAX;
            self.binary_page.log_lines.drain(..drain_to);
        }

        self.reconcile_node_lifecycle();

        let mut tasks: Vec<Task<Message>> = Vec::new();
        if btc_new {
            tasks.push(widget::operation::scroll_to(
                ui_render::bitcoin_scroll_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: f32::MAX,
                },
            ));
        }
        if els_new {
            tasks.push(widget::operation::scroll_to(
                ui_render::electrs_scroll_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: f32::MAX,
                },
            ));
        }
        if build_new && self.binary_page.disclosures.build_details {
            tasks.push(widget::operation::scroll_to(
                ui_render::build_scroll_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: f32::MAX,
                },
            ));
        }
        if build_events_pending {
            tasks.push(Task::done(Message::OutputTick));
        }

        if tasks.is_empty() {
            Task::none()
        } else {
            Task::batch(tasks)
        }
    }

    fn handle_rpc_tick(&mut self) -> Task<Message> {
        self.reconcile_node_lifecycle();
        if self.active_status_poll.is_some() {
            return Task::none();
        }

        let bitcoin_running = self.bitcoin_handle.is_some() && self.bitcoin_shutdown.is_none();
        let electrs_running = self.electrs_handle.is_some() && self.electrs_shutdown.is_none();
        if !bitcoin_running && !electrs_running {
            return Task::none();
        }

        let identity = StatusPollIdentity {
            request_id: self.next_status_poll,
            lifecycle_generation: self.lifecycle_generation,
        };
        self.next_status_poll = self.next_status_poll.wrapping_add(1).max(1);
        self.active_status_poll = Some(identity);

        let endpoints = self.managed_endpoints.clone();
        let managed_bitcoin_rpc = endpoints.as_ref().and_then(|snapshot| {
            self.active_rpc_addr
                .or_else(|| snapshot.rpc_candidates.first().copied())
                .map(|endpoint| (snapshot.cookie_file.clone(), endpoint))
        });

        Task::perform(
            async move {
                let (bitcoin_probe, electrs_status) = tokio::join!(
                    probe_managed_bitcoin(endpoints.as_ref(), bitcoin_running),
                    electrs_status::probe(electrs_running, managed_bitcoin_rpc),
                );

                StatusPollResult {
                    identity,
                    bitcoin_probe,
                    electrs_status,
                }
            },
            Message::StatusPollReceived,
        )
    }

    fn apply_status_poll(&mut self, result: StatusPollResult) {
        self.reconcile_node_lifecycle();
        if self.active_status_poll != Some(result.identity)
            || self.lifecycle_generation != result.identity.lifecycle_generation
        {
            return;
        }
        self.active_status_poll = None;

        if self.bitcoin_handle.is_some() && self.bitcoin_shutdown.is_none() {
            self.apply_bitcoin_probe(result.bitcoin_probe);
        } else {
            self.reset_bitcoin_service_status();
        }

        if self.electrs_handle.is_some() && self.electrs_shutdown.is_none() {
            let mut status = result.electrs_status;
            // Managed running state comes from the owned child handle, never
            // from a network probe that may observe another local service.
            status.running = true;
            if !self.bitcoin_ready() {
                status.connected = false;
                status.synced = false;
                status.ready = false;
            }
            self.apply_electrs_status(status);
        } else {
            self.electrs_status = ElectrsStatus::default();
        }
    }

    fn apply_bitcoin_probe(&mut self, probe: BitcoinProbeResult) {
        let previous_rpc_error = self.bitcoin_rpc_error.clone();
        let previous_p2p_error = self.bitcoin_p2p_error.clone();
        let previous_compatibility_error = self.bitcoin_compatibility_error.clone();

        self.bitcoin_rpc_reachable = probe.blockchain_info.is_ok() && probe.network_info.is_ok();
        self.bitcoin_rpc_error = match (&probe.blockchain_info, &probe.network_info) {
            (Ok(_), Ok(_)) => None,
            (Err(blockchain), Ok(_)) => Some(format!(
                "getblockchaininfo failed at the managed RPC endpoint: {blockchain}"
            )),
            (Ok(_), Err(network)) => Some(format!(
                "getnetworkinfo failed at the managed RPC endpoint: {network}"
            )),
            (Err(blockchain), Err(network)) if blockchain == network => Some(blockchain.clone()),
            (Err(blockchain), Err(network)) => Some(format!(
                "getblockchaininfo failed: {blockchain}; getnetworkinfo failed: {network}"
            )),
        };
        if let Some(endpoint) = probe.rpc_addr {
            self.active_rpc_addr = Some(endpoint);
        }

        if let Ok(info) = probe.blockchain_info {
            self.block_height = info.blocks;
            self.bitcoin_synced = !info.initial_block_download && info.blocks >= info.headers;
            self.bitcoin_compatibility_error = info.pruned.then(|| {
                "Bitcoin Core pruning is enabled, but managed Electrs requires prune=0. Disable pruning and rebuild the full chain data before launching again."
                    .to_owned()
            });
        } else {
            self.bitcoin_synced = false;
            self.block_height = 0;
            self.bitcoin_compatibility_error = None;
        }

        if let Ok(network) = probe.network_info {
            self.bitcoin_compatibility_error = self.bitcoin_compatibility_error.take().or_else(|| {
                (!network.network_active).then(|| {
                    "Bitcoin Core P2P networking is inactive. Remove networkactive=0 or call setnetworkactive true before launching Electrs."
                        .to_owned()
                })
            }).or_else(|| {
                (network.version < 210_000).then(|| {
                    format!(
                        "Bitcoin Core version {} is too old; managed Electrs requires Bitcoin Core 0.21 or newer.",
                        network.version
                    )
                })
            });
        }

        match probe.p2p_result {
            Ok(endpoint) => {
                self.bitcoin_p2p_reachable = true;
                self.bitcoin_p2p_error = None;
                self.active_p2p_addr = Some(endpoint);
            }
            Err(error) => {
                self.bitcoin_p2p_reachable = false;
                self.bitcoin_p2p_error = Some(error);
            }
        }

        for (previous, current, label) in [
            (
                previous_rpc_error.as_deref(),
                self.bitcoin_rpc_error.as_deref(),
                "RPC readiness",
            ),
            (
                previous_p2p_error.as_deref(),
                self.bitcoin_p2p_error.as_deref(),
                "P2P readiness",
            ),
            (
                previous_compatibility_error.as_deref(),
                self.bitcoin_compatibility_error.as_deref(),
                "Electrs compatibility",
            ),
        ] {
            if previous != current {
                if let Some(error) = current {
                    push_msg(
                        &self.bitcoin_queue,
                        &format!("Bitcoin {label} check failed: {error}"),
                    );
                } else if previous.is_some() {
                    push_msg(
                        &self.bitcoin_queue,
                        &format!("Bitcoin {label} check recovered."),
                    );
                }
            }
        }
    }

    fn reset_bitcoin_service_status(&mut self) {
        self.bitcoin_synced = false;
        self.bitcoin_rpc_reachable = false;
        self.bitcoin_p2p_reachable = false;
        self.bitcoin_rpc_error = None;
        self.bitcoin_p2p_error = None;
        self.bitcoin_compatibility_error = None;
        self.block_height = 0;
    }

    const fn bitcoin_ready(&self) -> bool {
        self.bitcoin_handle.is_some()
            && self.bitcoin_shutdown.is_none()
            && self.bitcoin_rpc_reachable
            && self.bitcoin_p2p_reachable
            && self.bitcoin_compatibility_error.is_none()
    }

    fn bitcoin_dependency_error(&self) -> Option<&str> {
        self.bitcoin_compatibility_error
            .as_deref()
            .or(self.bitcoin_rpc_error.as_deref())
            .or(self.bitcoin_p2p_error.as_deref())
    }

    fn reconcile_node_lifecycle(&mut self) {
        let bitcoin_shutdown_finished = self
            .bitcoin_shutdown
            .as_mut()
            .and_then(ShutdownWorker::join_if_finished);
        if let Some(join_result) = bitcoin_shutdown_finished {
            self.bitcoin_shutdown = None;
            self.bitcoin_running = false;
            self.reset_bitcoin_service_status();
            self.invalidate_electrs_dependency(
                "Bitcoin Core stopped; Electrs is no longer connected to its managed dependency.",
            );
            if join_result.is_err() {
                push_msg(
                    &self.bitcoin_queue,
                    "Bitcoin shutdown worker stopped unexpectedly.",
                );
            }
            self.advance_lifecycle_generation();
        }

        let electrs_shutdown_finished = self
            .electrs_shutdown
            .as_mut()
            .and_then(ShutdownWorker::join_if_finished);
        if let Some(join_result) = electrs_shutdown_finished {
            self.electrs_shutdown = None;
            self.electrs_status = ElectrsStatus::default();
            if join_result.is_err() {
                push_msg(
                    &self.electrs_queue,
                    "Electrs shutdown worker stopped unexpectedly.",
                );
            }
            self.advance_lifecycle_generation();
        }

        let bitcoin_exited = self
            .bitcoin_handle
            .as_mut()
            .is_some_and(|handle| !handle.is_running());
        if bitcoin_exited {
            self.bitcoin_handle = None;
            self.bitcoin_running = false;
            self.reset_bitcoin_service_status();
            self.invalidate_electrs_dependency(
                "Bitcoin Core exited; stop Electrs before starting a new Bitcoin generation.",
            );
            push_msg(&self.bitcoin_queue, "bitcoind has stopped.");
            self.advance_lifecycle_generation();
        }

        let electrs_exited = self
            .electrs_handle
            .as_mut()
            .is_some_and(|handle| !handle.is_running());
        if electrs_exited {
            self.electrs_handle = None;
            self.electrs_status = ElectrsStatus::default();
            push_msg(&self.electrs_queue, "electrs has stopped.");
            self.advance_lifecycle_generation();
        }

        if !self.node_lifecycle_active() {
            self.managed_endpoints = None;
            self.active_rpc_addr = None;
            self.active_p2p_addr = None;
        }
    }

    fn invalidate_electrs_dependency(&mut self, error: &str) {
        if self.electrs_handle.is_none() && self.electrs_shutdown.is_none() {
            return;
        }
        self.electrs_status.synced = false;
        self.electrs_status.connected = false;
        self.electrs_status.ready = false;
        self.electrs_status.bitcoin_blocks = None;
        self.electrs_status.bitcoin_headers = None;
        self.electrs_status.sync_percent = None;
        self.electrs_status.bitcoin_error = Some(error.to_owned());
        push_msg(&self.electrs_queue, error);
    }

    fn advance_lifecycle_generation(&mut self) {
        self.lifecycle_generation = self.lifecycle_generation.wrapping_add(1).max(1);
        self.active_status_poll = None;
    }

    const fn paths_are_editable(&self) -> bool {
        self.binary_page.active_operation.is_none()
            && self.pending_path_save.is_none()
            && !self.node_lifecycle_active()
    }

    const fn node_lifecycle_active(&self) -> bool {
        self.bitcoin_handle.is_some()
            || self.electrs_handle.is_some()
            || self.bitcoin_shutdown.is_some()
            || self.electrs_shutdown.is_some()
    }

    fn bitcoin_rpc_auth(&self) -> Result<RpcAuth, String> {
        let endpoints = self
            .managed_endpoints
            .as_ref()
            .ok_or_else(|| "managed Bitcoin endpoint snapshot is missing".to_owned())?;
        let endpoint = self
            .active_rpc_addr
            .or_else(|| endpoints.rpc_candidates.first().copied())
            .ok_or_else(|| "managed Bitcoin RPC endpoint is missing".to_owned())?;
        RpcAuth::from_managed_cookie(&endpoints.cookie_file, endpoint)
            .map_err(|error| error.to_string())
    }

    fn invalidate_binary_inventory(&mut self) {
        self.binary_page.inventory_request = None;
        self.binary_page.installed_versions = None;
        self.binary_page.available_versions = None;
        self.binary_page.installed_load = InventoryLoad::Idle;
        self.binary_page.available_load = InventoryLoad::Idle;
        self.binary_page.selected_bitcoin = None;
        self.binary_page.selected_electrs = None;
    }

    fn save_paths(&mut self) -> Task<Message> {
        if !self.paths_are_editable() {
            self.overlay_message = Some(
                "Paths cannot be changed while a node is running or a binary build or path save is active."
                    .to_owned(),
            );
            return Task::none();
        }

        let bins = self.binaries_path_edit.trim().to_owned();
        let btc = self.bitcoin_data_path_edit.trim().to_owned();
        let els = self.electrs_data_path_edit.trim().to_owned();

        if bins.is_empty() || btc.is_empty() || els.is_empty() {
            self.overlay_message = Some("All path fields must be filled in.".into());
            return Task::none();
        }

        let candidate = Config {
            binaries_path: PathBuf::from(&bins),
            bitcoin_data_path: PathBuf::from(&btc),
            electrs_data_path: PathBuf::from(&els),
        };
        if let Err(error) = candidate.validate_paths() {
            self.overlay_message = Some(format!("Invalid paths:\n{error}"));
            return Task::none();
        }

        let request_id = self.next_path_save;
        self.next_path_save = self.next_path_save.wrapping_add(1).max(1);
        self.pending_path_save = Some(request_id);

        Task::perform(
            async move {
                platform::prepare_real_directory(
                    &candidate.bitcoin_data_path,
                    "Bitcoin data directory",
                    true,
                )
                .map_err(|error| error.to_string())?;
                platform::prepare_real_directory(
                    &candidate.electrs_data_path,
                    "electrs database directory",
                    true,
                )
                .map_err(|error| error.to_string())?;
                candidate.save().map_err(|e| e.to_string())?;
                Ok(candidate)
            },
            move |result| Message::PathsSaved { request_id, result },
        )
    }

    fn apply_paths_saved(
        &mut self,
        request_id: u64,
        result: Result<Config, String>,
    ) -> Task<Message> {
        if self.pending_path_save != Some(request_id) {
            return Task::none();
        }
        self.pending_path_save = None;

        match result {
            Ok(config)
                if self.binary_page.active_operation.is_none() && !self.node_lifecycle_active() =>
            {
                self.config = config;
                self.binaries_path_edit = self.config.binaries_path.to_string_lossy().into_owned();
                self.bitcoin_data_path_edit =
                    self.config.bitcoin_data_path.to_string_lossy().into_owned();
                self.electrs_data_path_edit =
                    self.config.electrs_data_path.to_string_lossy().into_owned();
                push_msg(&self.bitcoin_queue, "--- Paths updated ---");
                push_msg(
                    &self.bitcoin_queue,
                    &format!("Binaries : {}", self.config.binaries_path.display()),
                );
                push_msg(
                    &self.bitcoin_queue,
                    &format!("Data dir : {}", self.config.bitcoin_data_path.display()),
                );
                push_msg(&self.electrs_queue, "--- Paths updated ---");
                push_msg(
                    &self.electrs_queue,
                    &format!("DB dir   : {}", self.config.electrs_data_path.display()),
                );
                self.overlay_message = Some(format!(
                    "Paths saved.\nChanges take effect on the next node launch.\n\nConfig: {}",
                    Config::config_file_path().display()
                ));
                self.invalidate_binary_inventory();
                if self.page == Page::Binaries {
                    self.refresh_binary_info()
                } else {
                    Task::none()
                }
            }
            Ok(_) => {
                self.overlay_message = Some(
                    "Paths were saved but were not applied while a node or binary build is active."
                        .to_owned(),
                );
                Task::none()
            }
            Err(e) => {
                self.overlay_message = Some(format!("Failed to save paths:\n{e}"));
                Task::none()
            }
        }
    }

    fn launch_bitcoin(&mut self) -> Task<Message> {
        self.reconcile_node_lifecycle();
        if self.binary_page.active_operation.is_some() {
            self.overlay_message = Some(
                "Wait for the active binary update to finish before launching Bitcoin.".into(),
            );
            return Task::none();
        }
        if self.pending_path_save.is_some() {
            self.overlay_message =
                Some("Wait for the pending path save before launching Bitcoin.".to_owned());
            return Task::none();
        }
        if self.bitcoin_shutdown.is_some() {
            self.overlay_message = Some("Bitcoin is still shutting down.".to_owned());
            return Task::none();
        }
        if self.bitcoin_handle.is_some() {
            self.overlay_message = Some("Bitcoin is already running.".to_owned());
            return Task::none();
        }
        if self.electrs_handle.is_some() || self.electrs_shutdown.is_some() {
            self.overlay_message = Some(
                "Electrs still belongs to the previous Bitcoin generation. Stop Electrs before launching Bitcoin again."
                    .to_owned(),
            );
            return Task::none();
        }
        if !self.ensure_binary_installation_ready() {
            return Task::none();
        }
        if let Err(e) = rpc::ensure_bitcoin_conf(&self.config.bitcoin_data_path) {
            self.overlay_message = Some(format!("Failed to prepare Bitcoin config:\n{e}"));
            return Task::none();
        }
        let endpoints = match resolve_managed_endpoints(&self.config.bitcoin_data_path) {
            Ok(endpoints) => endpoints,
            Err(error) => {
                self.overlay_message = Some(format!(
                    "Bitcoin networking configuration is incompatible with managed Electrs:\n{error:#}\n\nBitEngine did not change bitcoin.conf or settings.json. Correct the reported setting and launch again."
                ));
                return Task::none();
            }
        };

        match process_manager::launch_bitcoind(
            &self.config.binaries_path,
            &self.config.bitcoin_data_path,
            endpoints.rpc_port,
            &self.bitcoin_queue,
        ) {
            Ok(handle) => {
                self.bitcoin_handle = Some(handle);
                self.managed_endpoints = Some(endpoints);
                self.active_rpc_addr = None;
                self.active_p2p_addr = None;
                self.bitcoin_running = true;
                self.reset_bitcoin_service_status();
                self.advance_lifecycle_generation();
                return Task::done(Message::RpcTick);
            }
            Err(e) => {
                push_msg(&self.bitcoin_queue, &format!("Launch error: {e}"));
                self.overlay_message = Some(format!("Failed to launch Bitcoin:\n{e}"));
            }
        }
        Task::none()
    }

    fn launch_electrs(&mut self) -> Task<Message> {
        self.reconcile_node_lifecycle();
        if self.binary_page.active_operation.is_some() {
            self.overlay_message = Some(
                "Wait for the active binary update to finish before launching electrs.".into(),
            );
            return Task::none();
        }
        if self.pending_path_save.is_some() {
            self.overlay_message =
                Some("Wait for the pending path save before launching electrs.".to_owned());
            return Task::none();
        }
        if self.electrs_shutdown.is_some() {
            self.overlay_message = Some("Electrs is still shutting down.".to_owned());
            return Task::none();
        }
        if self.electrs_handle.is_some() {
            self.overlay_message = Some("Electrs is already running.".to_owned());
            return Task::none();
        }
        if self.bitcoin_handle.is_none() || self.bitcoin_shutdown.is_some() {
            self.overlay_message = Some(
                "Bitcoin must be running before starting Electrs.\n\
                 Launch Bitcoin first and wait for its RPC and P2P services."
                    .into(),
            );
            return Task::none();
        }
        if !self.bitcoin_ready() {
            let reason = self.bitcoin_dependency_error().unwrap_or(
                "Bitcoin RPC and P2P readiness have not yet been confirmed for this generation.",
            );
            self.overlay_message = Some(format!(
                "Bitcoin is running but is not usable by managed Electrs:\n{reason}\n\nWait for recovery or correct the reported Bitcoin setting, then retry."
            ));
            return Task::none();
        }
        if !self.ensure_binary_installation_ready() {
            return Task::none();
        }
        let Some(endpoints) = self.managed_endpoints.as_ref() else {
            self.overlay_message = Some(
                "Bitcoin endpoint state is incomplete; restart Bitcoin before launching Electrs."
                    .to_owned(),
            );
            return Task::none();
        };
        let (Some(rpc_addr), Some(p2p_addr)) = (self.active_rpc_addr, self.active_p2p_addr) else {
            self.overlay_message = Some(
                "Bitcoin endpoint readiness is incomplete; wait for the next status check before launching Electrs."
                    .to_owned(),
            );
            return Task::none();
        };

        match process_manager::launch_electrs(
            &self.config.binaries_path,
            &self.config.bitcoin_data_path,
            &self.config.electrs_data_path,
            Config::electrum_addr(),
            ElectrsBitcoinConnection {
                rpc_addr,
                p2p_addr,
                cookie_file: &endpoints.cookie_file,
            },
            &self.electrs_queue,
        ) {
            Ok(handle) => {
                self.electrs_handle = Some(handle);
                self.electrs_status = ElectrsStatus {
                    running: true,
                    ..ElectrsStatus::default()
                };
                self.advance_lifecycle_generation();
            }
            Err(e) => {
                push_msg(&self.electrs_queue, &format!("Launch error: {e}"));
                self.overlay_message = Some(format!("Failed to launch Electrs:\n{e}"));
            }
        }
        Task::none()
    }

    fn shutdown_both(&mut self) -> Task<Message> {
        self.reconcile_node_lifecycle();
        self.terminate_electrs_internal();

        if self.bitcoin_shutdown.is_none() {
            let auth = self.bitcoin_rpc_auth().ok();
            let btc_q = Arc::clone(&self.bitcoin_queue);
            if let Some(mut handle) = self.bitcoin_handle.take() {
                push_msg(&btc_q, "Sending stop via RPC…");
                self.bitcoin_running = true;
                self.reset_bitcoin_service_status();
                self.advance_lifecycle_generation();
                match ShutdownWorker::spawn("bitengine-bitcoin-shutdown", move |force| {
                    let stopped_via_rpc = if force.load(Ordering::Acquire) {
                        false
                    } else if let Some(auth) = auth {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .is_ok_and(|runtime| {
                                runtime.block_on(async {
                                    tokio::select! {
                                        result = rpc::stop_bitcoind(&auth) => result.is_ok(),
                                        () = wait_for_shutdown_force(&force) => false,
                                    }
                                })
                            })
                    } else {
                        false
                    };

                    if stopped_via_rpc {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(60);
                        while handle.is_running() {
                            if force.load(Ordering::Acquire)
                                || std::time::Instant::now() >= deadline
                            {
                                handle.force_terminate();
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
                    } else {
                        handle.terminate_interruptibly(&force);
                    }
                    push_msg(&btc_q, "bitcoind stopped.");
                }) {
                    Ok(worker) => self.bitcoin_shutdown = Some(worker),
                    Err(error) => {
                        self.bitcoin_running = false;
                        self.reset_bitcoin_service_status();
                        push_msg(
                            &self.bitcoin_queue,
                            &format!("Could not start Bitcoin shutdown worker: {error}"),
                        );
                        self.advance_lifecycle_generation();
                    }
                }
            } else {
                self.bitcoin_running = false;
                self.reset_bitcoin_service_status();
            }
        }

        Task::none()
    }

    fn shutdown_electrs_only(&mut self) -> Task<Message> {
        self.terminate_electrs_internal();
        Task::none()
    }

    fn refresh_binary_info(&mut self) -> Task<Message> {
        if self.binary_page.active_operation.is_some() {
            self.binary_page.error =
                Some("Installed binary inventory is locked while an update is active.".to_owned());
            return Task::none();
        }
        if !self.ensure_binary_installation_ready() {
            self.binary_page.installed_load = InventoryLoad::Idle;
            self.binary_page.available_load = InventoryLoad::Idle;
            return Task::none();
        }
        let request_id = self.next_inventory_request;
        self.next_inventory_request = self.next_inventory_request.wrapping_add(1).max(1);
        self.binary_page.inventory_request = Some(request_id);
        self.binary_page.installed_load = InventoryLoad::Loading;
        self.binary_page.available_load = InventoryLoad::Loading;
        let binaries_dir = self.config.binaries_path.clone();
        Task::batch([
            Task::perform(
                async move { InstalledVersions::detect(&binaries_dir).await },
                move |versions| Message::InstalledVersionsLoaded {
                    request_id,
                    versions,
                },
            ),
            Task::perform(AvailableVersions::fetch(), move |versions| {
                Message::AvailableVersionsLoaded {
                    request_id,
                    versions,
                }
            }),
        ])
    }

    fn apply_available_versions(&mut self, versions: AvailableVersions) {
        self.binary_page.available_load = InventoryLoad::Idle;
        if let Ok(releases) = &versions.bitcoin {
            if !self
                .binary_page
                .selected_bitcoin
                .as_ref()
                .is_some_and(|selected| releases.contains(selected))
            {
                self.binary_page.selected_bitcoin = releases.first().cloned();
            }
        }
        if let Ok(releases) = &versions.electrs {
            if !self
                .binary_page
                .selected_electrs
                .as_ref()
                .is_some_and(|selected| releases.contains(selected))
            {
                self.binary_page.selected_electrs = releases.first().cloned();
            }
        }
        self.binary_page.available_versions = Some(versions);
    }

    fn start_build(&mut self, kind: BinaryKind) -> Task<Message> {
        if !self.ensure_binary_installation_ready() {
            return Task::none();
        }
        if self.pending_path_save.is_some() {
            self.binary_page.error = Some(
                "Wait for the pending path save to finish before starting a build.".to_owned(),
            );
            return Task::none();
        }
        if self.binary_page.active_operation.is_some() {
            self.binary_page.error = Some(
                "A build is already active. Wait for it to finish or cancel it first.".to_owned(),
            );
            return Task::none();
        }

        let selected = match kind {
            BinaryKind::BitcoinCore => self.binary_page.selected_bitcoin.clone(),
            BinaryKind::Electrs => self.binary_page.selected_electrs.clone(),
        };
        let Some(version) = selected else {
            self.binary_page.error = Some(
                "No stable release is available. Refresh the release information and try again."
                    .to_owned(),
            );
            return Task::none();
        };

        let operation_id = BuildOperationId(self.next_build_operation);
        self.next_build_operation = self.next_build_operation.wrapping_add(1).max(1);
        let binaries_dir = self.config.binaries_path.clone();
        let workspace = binaries::workspace_for(&binaries_dir);
        self.binary_page.active_operation = Some(operation_id);
        self.binary_page.displayed_operation = Some(operation_id);
        self.binary_page.active_kind = Some(kind);
        self.binary_page.displayed_request = Some(BuildRequestPresentation {
            operation_id,
            kind,
            version: version.clone(),
            binaries_dir: binaries_dir.clone(),
            workspace: workspace.clone(),
        });
        self.binary_page.stage = Some(BuildStage::CheckingRequirements);
        self.binary_page.progress = BuildStage::CheckingRequirements.progress();
        self.binary_page.log_lines.clear();
        self.binary_page.disclosures.build_details = false;
        self.binary_page.cancellation_requested = false;
        self.binary_page.error = None;
        self.binary_page.success = None;
        self.binary_page.last_log_path = None;

        let service = self.build_service.clone();
        let event_tx = self.build_event_tx.clone();
        let request = BuildRequest {
            operation_id,
            kind,
            version,
            binaries_dir,
            workspace,
            cores: build_worker_count(),
        };
        Task::perform(
            async move { service.run(request, event_tx).await },
            move |result| Message::BuildFinished {
                operation_id,
                result,
            },
        )
    }

    fn ensure_binary_installation_ready(&mut self) -> bool {
        match BuildService::ensure_installation_recovered(&self.config.binaries_path) {
            Ok(()) => true,
            Err(error) => {
                let message = format!(
                    "Binary installation recovery is required before inventory, launch, or update:\n{error}"
                );
                self.binary_page.error = Some(message.clone());
                self.overlay_message = Some(message);
                false
            }
        }
    }

    fn apply_build_event(&mut self, event: BuildEvent) {
        match event {
            BuildEvent::Stage {
                operation_id,
                kind,
                stage,
            } if self.binary_page.active_operation == Some(operation_id) => {
                self.binary_page.active_kind = Some(kind);
                self.binary_page.stage = Some(stage);
                self.binary_page.progress = stage.progress();
            }
            BuildEvent::Progress {
                operation_id,
                progress,
            } if self.binary_page.active_operation == Some(operation_id) => {
                self.binary_page.progress = progress.clamp(0.0, 1.0);
            }
            BuildEvent::Log {
                operation_id,
                message,
            } if self.binary_page.displayed_operation == Some(operation_id) => {
                self.binary_page
                    .log_lines
                    .extend(message.lines().map(ToOwned::to_owned));
            }
            BuildEvent::Stage { .. } | BuildEvent::Progress { .. } | BuildEvent::Log { .. } => {}
        }
    }

    fn apply_build_finished(
        &mut self,
        operation_id: BuildOperationId,
        result: Result<BuildSummary, BuildFailure>,
    ) -> Task<Message> {
        if self.binary_page.active_operation != Some(operation_id) {
            return Task::none();
        }
        self.binary_page.active_operation = None;
        self.binary_page.active_kind = None;
        self.binary_page.cancellation_requested = false;
        match result {
            Ok(summary) => {
                self.binary_page.stage = Some(BuildStage::Complete);
                self.binary_page.progress = 1.0;
                self.binary_page.last_log_path = Some(summary.log_path);
                self.binary_page.success = Some(format!(
                    "{} {} installed successfully ({}). The new binary will be used on the next launch.",
                    summary.kind.label(),
                    summary.version,
                    summary.installed.join(", ")
                ));
                self.binary_page.error = None;
                self.refresh_binary_info()
            }
            Err(failure) => {
                self.binary_page.error = Some(if failure.conflict {
                    format!("Build request declined: {}", failure.message)
                } else {
                    failure.message
                });
                if !failure.cancelled {
                    self.binary_page.disclosures.build_details =
                        !self.binary_page.log_lines.is_empty();
                }
                self.binary_page.success = None;
                Task::none()
            }
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn terminate_electrs_internal(&mut self) {
        if self.electrs_shutdown.is_some() {
            return;
        }
        if let Some(mut handle) = self.electrs_handle.take() {
            push_msg(&self.electrs_queue, "Terminating electrs…");
            let els_q = Arc::clone(&self.electrs_queue);
            self.electrs_status = ElectrsStatus {
                running: true,
                ..ElectrsStatus::default()
            };
            self.advance_lifecycle_generation();
            match ShutdownWorker::spawn("bitengine-electrs-shutdown", move |force| {
                handle.terminate_interruptibly(&force);
                push_msg(&els_q, "electrs stopped.");
            }) {
                Ok(worker) => self.electrs_shutdown = Some(worker),
                Err(error) => {
                    self.electrs_status = ElectrsStatus::default();
                    push_msg(
                        &self.electrs_queue,
                        &format!("Could not start Electrs shutdown worker: {error}"),
                    );
                    self.advance_lifecycle_generation();
                }
            }
        } else {
            self.electrs_status = ElectrsStatus::default();
        }
    }

    fn apply_electrs_status(&mut self, next_status: ElectrsStatus) {
        let previous = self.electrs_status.clone();

        if previous.metrics_error != next_status.metrics_error {
            if let Some(error) = next_status.metrics_error.as_deref() {
                push_msg(
                    &self.electrs_queue,
                    &format!("Electrs metrics check failed: {error}"),
                );
            } else if previous.metrics_error.is_some() {
                push_msg(&self.electrs_queue, "Electrs metrics check recovered.");
            }
        }

        if previous.bitcoin_error != next_status.bitcoin_error {
            if let Some(error) = next_status.bitcoin_error.as_deref() {
                push_msg(
                    &self.electrs_queue,
                    &format!("Electrs Bitcoin check failed: {error}"),
                );
            } else if previous.bitcoin_error.is_some() {
                push_msg(&self.electrs_queue, "Electrs Bitcoin check recovered.");
            }
        }

        if previous.connect_error != next_status.connect_error {
            if let Some(error) = next_status.connect_error.as_deref() {
                push_msg(
                    &self.electrs_queue,
                    &format!("Electrs connectivity check failed: {error}"),
                );
            } else if previous.connect_error.is_some() {
                push_msg(&self.electrs_queue, "Electrs connectivity check recovered.");
            }
        }

        if !previous.connected && next_status.connected {
            push_msg(
                &self.electrs_queue,
                "Electrs completed its managed Bitcoin connection and is answering Electrum protocol requests.",
            );
        }
        if !previous.ready && next_status.ready {
            push_msg(
                &self.electrs_queue,
                "Electrs is ready to serve BitEngine clients.",
            );
        }

        if !previous.synced && next_status.synced {
            let height = next_status.electrs_height.unwrap_or_default();
            push_msg(
                &self.electrs_queue,
                &format!("Electrs synced to Bitcoin Core at height {height}."),
            );
        } else if previous.synced != next_status.synced {
            if let (Some(index_height), Some(blocks)) =
                (next_status.electrs_height, next_status.bitcoin_blocks)
            {
                let headers = next_status
                    .bitcoin_headers
                    .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
                push_msg(
                    &self.electrs_queue,
                    &format!(
                        "Electrs not yet synced: indexed={index_height}, blocks={blocks}, headers={headers}."
                    ),
                );
            }
        }

        self.electrs_status = next_status;
    }

    // ── subscription ──────────────────────────────────────────────────────────

    pub fn subscription(_: &Self) -> Subscription<Message> {
        Subscription::batch([
            time::every(Duration::from_millis(100)).map(|_| Message::OutputTick),
            time::every(Duration::from_secs(5)).map(|_| Message::RpcTick),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        ui_render::view(self)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Wake both shutdown workers before waiting on any one resource so
        // their graceful waits are shortened concurrently during app exit.
        if let Some(worker) = &self.bitcoin_shutdown {
            worker.request_force();
        }
        if let Some(worker) = &self.electrs_shutdown {
            worker.request_force();
        }

        if let Some(mut handle) = self.electrs_handle.take() {
            handle.force_terminate();
        }
        if let Some(mut handle) = self.bitcoin_handle.take() {
            handle.force_terminate();
        }

        drop(self.electrs_shutdown.take());
        drop(self.bitcoin_shutdown.take());
    }
}

// ── Async helpers ─────────────────────────────────────────────────────────────

async fn wait_for_shutdown_force(force: &AtomicBool) {
    while !force.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn browse_folder(title: &str) -> Option<String> {
    rfd::AsyncFileDialog::new()
        .set_title(title)
        .pick_folder()
        .await
        .map(|f| f.path().to_string_lossy().into_owned())
}

// ── Queue helper ──────────────────────────────────────────────────────────────

fn push_msg(queue: &OutputQueue, msg: &str) {
    if let Ok(mut q) = queue.lock() {
        if q.len() >= 10_000 {
            q.pop_front();
        }
        q.push_back(msg.to_owned());
    }
}

fn log_binary_resolution(queue: &OutputQueue, binaries_path: &Path, binary: &str) {
    let resolved = binaries_path.join(platform::executable_name(binary));
    let status = if resolved.exists() {
        "found"
    } else {
        "missing"
    };
    push_msg(
        queue,
        &format!("Resolved {binary}: {} ({status})", resolved.display()),
    );
}

async fn probe_managed_bitcoin(
    endpoints: Option<&ManagedBitcoinEndpoints>,
    process_running: bool,
) -> BitcoinProbeResult {
    let Some(endpoints) = endpoints.filter(|_| process_running) else {
        let error = "Bitcoin Core is not running in this managed generation".to_owned();
        return BitcoinProbeResult {
            blockchain_info: Err(error.clone()),
            network_info: Err(error.clone()),
            rpc_addr: None,
            p2p_result: Err(error),
        };
    };

    let (rpc_probe, p2p_result) = tokio::join!(
        probe_managed_rpc(endpoints),
        bitcoin_status::probe_p2p_candidates(&endpoints.p2p_candidates),
    );
    BitcoinProbeResult {
        blockchain_info: rpc_probe.blockchain_info,
        network_info: rpc_probe.network_info,
        rpc_addr: rpc_probe.rpc_addr,
        p2p_result: p2p_result.map_err(|error| error.to_string()),
    }
}

struct ManagedRpcProbe {
    blockchain_info: Result<BlockchainInfo, String>,
    network_info: Result<NetworkInfo, String>,
    rpc_addr: Option<SocketAddr>,
}

async fn probe_managed_rpc(endpoints: &ManagedBitcoinEndpoints) -> ManagedRpcProbe {
    let mut failures = Vec::with_capacity(endpoints.rpc_candidates.len());
    for &endpoint in &endpoints.rpc_candidates {
        let auth = match RpcAuth::from_managed_cookie(&endpoints.cookie_file, endpoint) {
            Ok(auth) => auth,
            Err(error) => {
                let error = error.to_string();
                return ManagedRpcProbe {
                    blockchain_info: Err(error.clone()),
                    network_info: Err(error),
                    rpc_addr: None,
                };
            }
        };
        let (blockchain_info, network_info) = tokio::join!(
            rpc::get_blockchain_info(&auth),
            rpc::get_network_info(&auth),
        );
        if blockchain_info.is_ok() && network_info.is_ok() {
            return ManagedRpcProbe {
                blockchain_info: blockchain_info.map_err(|error| error.to_string()),
                network_info: network_info.map_err(|error| error.to_string()),
                rpc_addr: Some(endpoint),
            };
        }
        let blockchain_outcome = blockchain_info.as_ref().map_or_else(
            |error| format!("failed: {error}"),
            |_| "succeeded".to_owned(),
        );
        let network_outcome = network_info.as_ref().map_or_else(
            |error| format!("failed: {error}"),
            |_| "succeeded".to_owned(),
        );
        failures.push(format!(
            "{endpoint}: getblockchaininfo {blockchain_outcome}; getnetworkinfo {network_outcome}"
        ));
    }

    let error = format!(
        "no managed Bitcoin RPC endpoint was usable ({})",
        failures.join("; ")
    );
    ManagedRpcProbe {
        blockchain_info: Err(error.clone()),
        network_info: Err(error),
        rpc_addr: None,
    }
}

fn build_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |count| count.get().saturating_sub(1).clamp(1, 8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[cfg(unix)]
    use std::{os::unix::fs::PermissionsExt as _, sync::atomic::AtomicUsize, time::Instant};

    fn test_app(root: &Path) -> App {
        App::from_config(Config::defaults(root), None, root.join("build-state.json"))
    }

    fn blockchain_info(
        blocks: u64,
        headers: u64,
        initial_block_download: bool,
        pruned: bool,
    ) -> BlockchainInfo {
        BlockchainInfo {
            blocks,
            headers,
            verification_progress: 1.0,
            initial_block_download,
            pruned,
        }
    }

    fn healthy_bitcoin_probe(info: BlockchainInfo) -> BitcoinProbeResult {
        BitcoinProbeResult {
            blockchain_info: Ok(info),
            network_info: Ok(NetworkInfo {
                version: 300_000,
                network_active: true,
            }),
            rpc_addr: Some(SocketAddr::from(([127, 0, 0, 1], 8332))),
            p2p_result: Ok(SocketAddr::from(([127, 0, 0, 1], 8333))),
        }
    }

    fn failed_bitcoin_probe(error: &str) -> BitcoinProbeResult {
        BitcoinProbeResult {
            blockchain_info: Err(error.to_owned()),
            network_info: Err(error.to_owned()),
            rpc_addr: None,
            p2p_result: Err(error.to_owned()),
        }
    }

    fn apply_current_status(
        app: &mut App,
        bitcoin_probe: BitcoinProbeResult,
        electrs_status: ElectrsStatus,
    ) {
        let identity = StatusPollIdentity {
            request_id: app.next_status_poll,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(identity);
        app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe,
            electrs_status,
        });
    }

    fn ready_electrs_status() -> ElectrsStatus {
        ElectrsStatus {
            running: true,
            connected: true,
            synced: true,
            ready: true,
            electrs_height: Some(100),
            bitcoin_blocks: Some(100),
            bitcoin_headers: Some(100),
            sync_percent: Some(100.0),
            ..ElectrsStatus::default()
        }
    }

    #[cfg(unix)]
    fn mark_bitcoin_dependency_ready(app: &mut App) -> anyhow::Result<()> {
        let endpoints = app
            .managed_endpoints
            .as_ref()
            .context("managed endpoint snapshot")?;
        app.active_rpc_addr = endpoints.rpc_candidates.first().copied();
        app.active_p2p_addr = endpoints.p2p_candidates.first().copied();
        app.bitcoin_rpc_reachable = true;
        app.bitcoin_p2p_reachable = true;
        app.bitcoin_compatibility_error = None;
        Ok(())
    }

    fn activate_build(app: &mut App, operation_id: BuildOperationId, kind: BinaryKind) {
        app.binary_page.active_operation = Some(operation_id);
        app.binary_page.displayed_operation = Some(operation_id);
        app.binary_page.active_kind = Some(kind);
    }

    fn alternate_config(root: &Path) -> Config {
        let root = root.join("alternate");
        Config {
            binaries_path: root.join("Binaries"),
            bitcoin_data_path: root.join("BitcoinChain"),
            electrs_data_path: root.join("ElectrsDB"),
        }
    }

    async fn read_test_rpc_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Value> {
        const MAX_REQUEST_BYTES: usize = 64 * 1024;

        let mut request = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 1024];
        loop {
            let bytes_read = stream.read(&mut buffer).await?;
            anyhow::ensure!(bytes_read > 0, "test RPC client closed before its request");
            request.extend_from_slice(&buffer[..bytes_read]);
            anyhow::ensure!(
                request.len() <= MAX_REQUEST_BYTES,
                "test RPC request exceeded {MAX_REQUEST_BYTES} bytes"
            );

            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end])?;
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>())
                .transpose()?
                .context("test RPC request omitted Content-Length")?;
            let body_start = header_end + 4;
            let body_end = body_start + content_length;
            if request.len() >= body_end {
                return serde_json::from_slice(&request[body_start..body_end])
                    .context("parse test RPC request");
            }
        }
    }

    async fn spawn_test_rpc_server(
        network_succeeds: bool,
    ) -> anyhow::Result<(
        SocketAddr,
        tokio::task::JoinHandle<anyhow::Result<Vec<String>>>,
    )> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let mut methods = Vec::with_capacity(2);
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await?;
                let request = read_test_rpc_request(&mut stream).await?;
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .context("test RPC request omitted method")?;
                let body = match method {
                    "getblockchaininfo" => serde_json::json!({
                        "result": {
                            "blocks": 100,
                            "headers": 100,
                            "verificationprogress": 1.0,
                            "initialblockdownload": false,
                            "pruned": false
                        },
                        "error": null
                    }),
                    "getnetworkinfo" if network_succeeds => serde_json::json!({
                        "result": {
                            "version": 300_000,
                            "networkactive": true
                        },
                        "error": null
                    }),
                    "getnetworkinfo" => serde_json::json!({
                        "result": null,
                        "error": {"code": -1, "message": "partial candidate"}
                    }),
                    unexpected => anyhow::bail!("unexpected test RPC method {unexpected}"),
                }
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                methods.push(method.to_owned());
            }
            Ok(methods)
        });
        Ok((endpoint, server))
    }

    #[tokio::test]
    async fn managed_rpc_probe_skips_a_partially_usable_candidate() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (partial_endpoint, partial_server) = spawn_test_rpc_server(false).await?;
        let (healthy_endpoint, healthy_server) = spawn_test_rpc_server(true).await?;
        let endpoints = ManagedBitcoinEndpoints {
            rpc_candidates: vec![partial_endpoint, healthy_endpoint],
            rpc_port: partial_endpoint.port(),
            p2p_candidates: Vec::new(),
            cookie_file,
        };

        let probe = probe_managed_rpc(&endpoints).await;

        assert_eq!(probe.rpc_addr, Some(healthy_endpoint));
        assert!(probe.blockchain_info.is_ok());
        assert!(probe.network_info.is_ok());
        for server in [partial_server, healthy_server] {
            let mut methods = tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .context("test RPC server timed out")???;
            methods.sort_unstable();
            assert_eq!(methods, ["getblockchaininfo", "getnetworkinfo"]);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn install_node_helper(root: &Path, name: &str, pid_log: &Path) -> anyhow::Result<()> {
        let binaries = root.join("Binaries");
        std::fs::create_dir_all(&binaries)?;
        let quoted_log = format!("'{}'", pid_log.to_string_lossy().replace('\'', "'\"'\"'"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> {quoted_log}\n\
             trap 'exit 0' TERM INT\n\
             while :; do sleep 1; done\n"
        );
        let executable = binaries.join(name);
        std::fs::write(&executable, script)?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(unix)]
    fn install_stubborn_node_helper(root: &Path, name: &str, pid_log: &Path) -> anyhow::Result<()> {
        let binaries = root.join("Binaries");
        std::fs::create_dir_all(&binaries)?;
        let quoted_log = format!("'{}'", pid_log.to_string_lossy().replace('\'', "'\"'\"'"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" >> {quoted_log}\n\
             trap '' TERM INT\n\
             while :; do sleep 1; done\n"
        );
        let executable = binaries.join(name);
        std::fs::write(&executable, script)?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(unix)]
    fn wait_for_pids(pid_log: &Path, count: usize) -> anyhow::Result<Vec<i32>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(contents) = std::fs::read_to_string(pid_log) {
                let pids = contents
                    .lines()
                    .filter_map(|line| line.parse::<i32>().ok())
                    .collect::<Vec<_>>();
                if pids.len() >= count {
                    return Ok(pids);
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!("node helper did not report {count} process IDs");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn process_exists(process_id: i32) -> bool {
        // SAFETY: signal zero only queries a PID emitted by a helper process
        // created and owned by this test.
        let result = unsafe { libc::kill(process_id, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    #[cfg(unix)]
    fn assert_process_exits(process_id: i32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_exists(process_id) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_exists(process_id),
            "node helper {process_id} was left running"
        );
    }

    #[test]
    fn update_binaries_navigation_opens_native_page() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        assert_eq!(app.page, Page::Dashboard);
        drop(app.update(Message::OpenBinaries));
        assert_eq!(app.page, Page::Binaries);
        drop(app.update(Message::OpenDashboard));
        assert_eq!(app.page, Page::Dashboard);
        Ok(())
    }

    #[test]
    fn available_versions_select_the_latest_release() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let older = "v29.2"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;
        let latest = "v30.0"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;
        app.binary_page.installed_versions = Some(InstalledVersions {
            bitcoin: Ok(Some(older.clone())),
            electrs: Ok(None),
        });
        app.apply_available_versions(AvailableVersions {
            bitcoin: Ok(vec![latest.clone(), older]),
            electrs: Ok(vec!["v0.10.10"
                .parse::<ReleaseVersion>()
                .map_err(anyhow::Error::msg)?]),
        });
        assert_eq!(app.binary_page.selected_bitcoin, Some(latest));
        assert!(app.binary_page.selected_electrs.is_some());
        Ok(())
    }

    #[test]
    fn starting_a_target_sets_clear_progress_and_blocks_a_second_request() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.binary_page.selected_bitcoin = Some(
            "v30.0"
                .parse::<ReleaseVersion>()
                .map_err(anyhow::Error::msg)?,
        );
        drop(app.start_build(BinaryKind::BitcoinCore));
        assert_eq!(app.binary_page.active_operation, Some(BuildOperationId(1)));
        assert_eq!(app.binary_page.active_kind, Some(BinaryKind::BitcoinCore));
        assert_eq!(
            app.binary_page.stage,
            Some(BuildStage::CheckingRequirements)
        );
        drop(app.start_build(BinaryKind::Electrs));
        assert!(app
            .binary_page
            .error
            .as_deref()
            .is_some_and(|message| message.contains("already active")));
        Ok(())
    }

    #[test]
    fn active_build_and_pending_save_lock_all_path_messages() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let initial_binaries = app.binaries_path_edit.clone();
        let initial_bitcoin = app.bitcoin_data_path_edit.clone();
        let initial_electrs = app.electrs_data_path_edit.clone();
        activate_build(&mut app, BuildOperationId(2), BinaryKind::BitcoinCore);

        drop(app.update(Message::BinariesPathChanged("/tmp/changed".to_owned())));
        drop(app.update(Message::BitcoinDataBrowsed(Some("/tmp/browsed".to_owned()))));
        drop(app.update(Message::ElectrsDataPathChanged(
            "/tmp/changed-electrs".to_owned(),
        )));
        drop(app.update(Message::SavePaths));

        assert_eq!(app.binaries_path_edit, initial_binaries);
        assert_eq!(app.bitcoin_data_path_edit, initial_bitcoin);
        assert_eq!(app.electrs_data_path_edit, initial_electrs);
        assert_eq!(app.pending_path_save, None);
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("build or path save is active")));

        let initial_config_binaries = app.config.binaries_path.clone();
        app.pending_path_save = Some(7);
        drop(app.apply_paths_saved(7, Ok(alternate_config(temporary.path()))));
        assert_eq!(app.config.binaries_path, initial_config_binaries);
        assert_eq!(app.pending_path_save, None);

        app.binary_page.active_operation = None;
        app.binary_page.active_kind = None;
        app.pending_path_save = Some(8);
        drop(app.update(Message::BinariesBrowsed(Some(
            "/tmp/late-browser-result".to_owned(),
        ))));
        assert_eq!(app.binaries_path_edit, initial_binaries);
        Ok(())
    }

    #[test]
    fn active_build_blocks_destination_launch_and_inventory() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        activate_build(&mut app, BuildOperationId(9), BinaryKind::BitcoinCore);

        drop(app.launch_bitcoin());
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("update to finish")));
        app.overlay_message = None;
        drop(app.launch_electrs());
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("update to finish")));

        app.binary_page.inventory_request = None;
        drop(app.refresh_binary_info());
        assert_eq!(app.binary_page.inventory_request, None);
        assert!(app
            .binary_page
            .error
            .as_deref()
            .is_some_and(|message| message.contains("inventory is locked")));
        Ok(())
    }

    #[test]
    fn path_candidate_is_applied_only_after_matching_save_success() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let initial_binaries = app.config.binaries_path.clone();
        let candidate = alternate_config(temporary.path());
        app.binaries_path_edit = candidate.binaries_path.display().to_string();
        app.bitcoin_data_path_edit = candidate.bitcoin_data_path.display().to_string();
        app.electrs_data_path_edit = candidate.electrs_data_path.display().to_string();

        let save_task = app.save_paths();
        let request_id = app
            .pending_path_save
            .context("path save should be pending")?;
        assert_eq!(app.config.binaries_path, initial_binaries);
        drop(save_task);

        drop(app.apply_paths_saved(request_id.wrapping_add(1), Ok(candidate.clone())));
        assert_eq!(app.config.binaries_path, initial_binaries);
        assert_eq!(app.pending_path_save, Some(request_id));

        drop(app.apply_paths_saved(request_id, Err("disk full".to_owned())));
        assert_eq!(app.config.binaries_path, initial_binaries);
        assert_eq!(app.pending_path_save, None);
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("disk full")));

        app.pending_path_save = Some(17);
        drop(app.apply_paths_saved(17, Ok(candidate.clone())));
        assert_eq!(app.config.binaries_path, candidate.binaries_path);
        assert_eq!(app.config.bitcoin_data_path, candidate.bitcoin_data_path);
        assert_eq!(app.config.electrs_data_path, candidate.electrs_data_path);
        Ok(())
    }

    #[test]
    fn pending_path_save_blocks_build_start() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.binary_page.selected_bitcoin = Some(
            "v30.0"
                .parse::<ReleaseVersion>()
                .map_err(anyhow::Error::msg)?,
        );
        app.pending_path_save = Some(3);

        drop(app.start_build(BinaryKind::BitcoinCore));

        assert_eq!(app.binary_page.active_operation, None);
        assert!(app
            .binary_page
            .error
            .as_deref()
            .is_some_and(|message| message.contains("pending path save")));
        Ok(())
    }

    #[test]
    fn pending_path_save_blocks_node_launch() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.pending_path_save = Some(4);

        drop(app.launch_bitcoin());

        assert!(app.bitcoin_handle.is_none());
        assert!(!app.bitcoin_running);
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("pending path save")));
        Ok(())
    }

    #[test]
    fn inventory_results_are_correlated_to_the_latest_request() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        drop(app.refresh_binary_info());
        let stale_request = app
            .binary_page
            .inventory_request
            .context("first inventory request should exist")?;
        drop(app.refresh_binary_info());
        let current_request = app
            .binary_page
            .inventory_request
            .context("second inventory request should exist")?;

        drop(app.update(Message::InstalledVersionsLoaded {
            request_id: stale_request,
            versions: InstalledVersions {
                bitcoin: Err("stale installed failure".to_owned()),
                electrs: Err("stale installed failure".to_owned()),
            },
        }));
        drop(app.update(Message::AvailableVersionsLoaded {
            request_id: stale_request,
            versions: AvailableVersions {
                bitcoin: Err("stale release failure".to_owned()),
                electrs: Err("stale release failure".to_owned()),
            },
        }));
        assert!(app.binary_page.installed_versions.is_none());
        assert!(app.binary_page.available_versions.is_none());

        let bitcoin = "v30.0"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;
        let electrs = "v0.10.10"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;
        drop(app.update(Message::InstalledVersionsLoaded {
            request_id: current_request,
            versions: InstalledVersions {
                bitcoin: Ok(None),
                electrs: Ok(None),
            },
        }));
        drop(app.update(Message::AvailableVersionsLoaded {
            request_id: current_request,
            versions: AvailableVersions {
                bitcoin: Ok(vec![bitcoin.clone()]),
                electrs: Ok(vec![electrs.clone()]),
            },
        }));

        assert!(app.binary_page.installed_versions.is_some());
        assert_eq!(app.binary_page.selected_bitcoin, Some(bitcoin));
        assert_eq!(app.binary_page.selected_electrs, Some(electrs));
        Ok(())
    }

    #[test]
    fn successful_path_change_invalidates_prior_inventory_request() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        drop(app.refresh_binary_info());
        let stale_request = app
            .binary_page
            .inventory_request
            .context("inventory request should exist")?;
        app.pending_path_save = Some(5);

        drop(app.apply_paths_saved(5, Ok(alternate_config(temporary.path()))));
        drop(app.update(Message::InstalledVersionsLoaded {
            request_id: stale_request,
            versions: InstalledVersions {
                bitcoin: Ok(None),
                electrs: Ok(None),
            },
        }));
        drop(app.update(Message::AvailableVersionsLoaded {
            request_id: stale_request,
            versions: AvailableVersions {
                bitcoin: Err("stale release response".to_owned()),
                electrs: Err("stale release response".to_owned()),
            },
        }));

        assert_eq!(app.binary_page.inventory_request, None);
        assert!(app.binary_page.installed_versions.is_none());
        assert!(app.binary_page.available_versions.is_none());
        Ok(())
    }

    #[test]
    fn installing_and_complete_stages_cannot_be_cancelled() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        activate_build(&mut app, BuildOperationId(6), BinaryKind::Electrs);
        app.binary_page.stage = Some(BuildStage::Compiling);
        assert!(app.binary_page.can_cancel());

        app.binary_page.stage = Some(BuildStage::Installing);
        assert!(!app.binary_page.can_cancel());
        app.binary_page.stage = Some(BuildStage::Complete);
        assert!(!app.binary_page.can_cancel());
        Ok(())
    }

    #[test]
    fn build_presentation_retains_requested_version_and_path_snapshot() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let requested_version = "v30.0"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;
        let requested_binaries = app.config.binaries_path.clone();
        let requested_workspace = binaries::workspace_for(&requested_binaries);
        app.binary_page.selected_bitcoin = Some(requested_version.clone());

        drop(app.start_build(BinaryKind::BitcoinCore));
        app.config = alternate_config(temporary.path());
        app.binary_page.selected_bitcoin = Some(
            "v29.2"
                .parse::<ReleaseVersion>()
                .map_err(anyhow::Error::msg)?,
        );

        let displayed = app
            .binary_page
            .displayed_request
            .as_ref()
            .context("active request should remain visible")?;
        assert_eq!(displayed.version, requested_version);
        assert_eq!(displayed.binaries_dir, requested_binaries);
        assert_eq!(displayed.workspace, requested_workspace);
        Ok(())
    }

    #[test]
    fn build_events_update_human_readable_status_and_log() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let operation_id = BuildOperationId(7);
        activate_build(&mut app, operation_id, BinaryKind::Electrs);
        app.apply_build_event(BuildEvent::Stage {
            operation_id,
            kind: BinaryKind::Electrs,
            stage: BuildStage::Compiling,
        });
        app.apply_build_event(BuildEvent::Progress {
            operation_id,
            progress: 0.7,
        });
        app.apply_build_event(BuildEvent::Log {
            operation_id,
            message: "Compiling crate\nFinished\n".to_owned(),
        });
        assert_eq!(app.binary_page.stage, Some(BuildStage::Compiling));
        assert!((app.binary_page.progress - 0.7).abs() < f32::EPSILON);
        assert_eq!(
            app.binary_page.log_lines,
            vec!["Compiling crate", "Finished"]
        );
        Ok(())
    }

    #[test]
    fn build_failure_clears_active_state_and_preserves_diagnostic() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let operation_id = BuildOperationId(11);
        activate_build(&mut app, operation_id, BinaryKind::BitcoinCore);
        drop(app.apply_build_finished(
            operation_id,
            Err(BuildFailure {
                message:
                    "compiler exited with status 2; installed binaries were not changed".to_owned(),
                cancelled: false,
                conflict: false,
            }),
        ));
        assert_eq!(app.binary_page.active_operation, None);
        assert_eq!(app.binary_page.active_kind, None);
        assert!(app
            .binary_page
            .error
            .as_deref()
            .is_some_and(|message| message.contains("not changed")));
        Ok(())
    }

    #[test]
    fn terminal_failure_cannot_be_undone_by_queued_stage_or_progress() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let operation_id = BuildOperationId(21);
        activate_build(&mut app, operation_id, BinaryKind::BitcoinCore);
        app.binary_page.stage = Some(BuildStage::Compiling);
        app.binary_page.progress = 0.7;

        drop(app.apply_build_finished(
            operation_id,
            Err(BuildFailure {
                message: "installation failed; installed binaries were not changed".to_owned(),
                cancelled: false,
                conflict: false,
            }),
        ));
        app.apply_build_event(BuildEvent::Stage {
            operation_id,
            kind: BinaryKind::BitcoinCore,
            stage: BuildStage::Installing,
        });
        app.apply_build_event(BuildEvent::Progress {
            operation_id,
            progress: 0.94,
        });
        app.apply_build_event(BuildEvent::Log {
            operation_id,
            message: "late diagnostic\n".to_owned(),
        });

        assert_eq!(app.binary_page.active_operation, None);
        assert_eq!(app.binary_page.active_kind, None);
        assert_eq!(app.binary_page.stage, Some(BuildStage::Compiling));
        assert!((app.binary_page.progress - 0.7).abs() < f32::EPSILON);
        assert_eq!(app.binary_page.log_lines, vec!["late diagnostic"]);
        assert!(app.binary_page.error.is_some());
        Ok(())
    }

    #[test]
    fn stale_prior_events_and_finish_are_ignored_during_retry() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let old_operation = BuildOperationId(31);
        let retry_operation = BuildOperationId(32);
        activate_build(&mut app, retry_operation, BinaryKind::Electrs);
        app.binary_page.stage = Some(BuildStage::Compiling);
        app.binary_page.progress = 0.6;

        app.apply_build_event(BuildEvent::Stage {
            operation_id: old_operation,
            kind: BinaryKind::BitcoinCore,
            stage: BuildStage::Complete,
        });
        app.apply_build_event(BuildEvent::Progress {
            operation_id: old_operation,
            progress: 1.0,
        });
        app.apply_build_event(BuildEvent::Log {
            operation_id: old_operation,
            message: "stale log\n".to_owned(),
        });
        drop(app.apply_build_finished(
            old_operation,
            Err(BuildFailure {
                message: "stale failure".to_owned(),
                cancelled: false,
                conflict: false,
            }),
        ));

        assert_eq!(app.binary_page.active_operation, Some(retry_operation));
        assert_eq!(app.binary_page.active_kind, Some(BinaryKind::Electrs));
        assert_eq!(app.binary_page.stage, Some(BuildStage::Compiling));
        assert!((app.binary_page.progress - 0.6).abs() < f32::EPSILON);
        assert!(app.binary_page.log_lines.is_empty());
        assert!(app.binary_page.error.is_none());
        Ok(())
    }

    #[test]
    fn build_event_transport_has_a_fixed_capacity() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let app = test_app(temporary.path());
        let operation_id = BuildOperationId(41);

        for index in 0..BUILD_EVENT_QUEUE_CAPACITY {
            app.build_event_tx.try_send(BuildEvent::Log {
                operation_id,
                message: format!("line {index}"),
            })?;
        }
        assert!(matches!(
            app.build_event_tx.try_send(BuildEvent::Log {
                operation_id,
                message: "overflow".to_owned(),
            }),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn spawned_bitcoin_process_cannot_launch_electrs_before_services_are_ready(
    ) -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());

        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        assert!(app.bitcoin_running);
        assert!(!app.bitcoin_ready());

        drop(app.launch_electrs());

        assert!(app.electrs_handle.is_none());
        assert!(!electrs_pid_log.exists());
        assert!(app.overlay_message.as_deref().is_some_and(|message| {
            message.contains("not usable by managed Electrs")
                && message.contains("readiness have not yet been confirmed")
        }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn incompatible_listen_setting_prevents_a_partial_launch() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        let bitcoin_data = temporary.path().join("BitcoinChain");
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::write(bitcoin_data.join("bitcoin.conf"), "listen=0\n")?;
        let mut app = test_app(temporary.path());

        drop(app.launch_bitcoin());

        assert!(app.bitcoin_handle.is_none());
        assert!(!bitcoin_pid_log.exists());
        assert!(app.overlay_message.as_deref().is_some_and(|message| {
            message.contains("incompatible with managed Electrs")
                && message.contains("listen=0")
                && message.contains("did not change bitcoin.conf or settings.json")
        }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn pruned_core_is_reported_before_electrs_is_spawned() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, false, true,
        )));

        assert!(!app.bitcoin_ready());
        drop(app.launch_electrs());

        assert!(app.electrs_handle.is_none());
        assert!(!electrs_pid_log.exists());
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("prune=0")));
        Ok(())
    }

    #[test]
    fn bitcoin_sync_and_compatibility_follow_electrs_prerequisites() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());

        app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, true, false,
        )));
        assert!(!app.bitcoin_synced, "IBD must remain unsynchronized");

        app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            99, 100, false, false,
        )));
        assert!(
            !app.bitcoin_synced,
            "a one-block lag must remain unsynchronized"
        );

        app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, false, false,
        )));
        assert!(app.bitcoin_synced);
        assert!(app.bitcoin_compatibility_error.is_none());

        app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, false, true,
        )));
        assert!(app
            .bitcoin_compatibility_error
            .as_deref()
            .is_some_and(|error| error.contains("prune=0")));

        let mut inactive = healthy_bitcoin_probe(blockchain_info(100, 100, false, false));
        inactive.network_info = Ok(NetworkInfo {
            version: 300_000,
            network_active: false,
        });
        app.apply_bitcoin_probe(inactive);
        assert!(app
            .bitcoin_compatibility_error
            .as_deref()
            .is_some_and(|error| error.contains("setnetworkactive true")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dependency_failure_is_reported_and_a_current_generation_recovery_clears_it(
    ) -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let _ = wait_for_pids(&electrs_pid_log, 1)?;

        apply_current_status(
            &mut app,
            failed_bitcoin_probe("RPC connection refused"),
            ElectrsStatus {
                running: true,
                metrics_error: Some("metrics connection refused".to_owned()),
                bitcoin_error: Some("Bitcoin Core unavailable: connection refused".to_owned()),
                connect_error: Some("Electrum protocol unavailable".to_owned()),
                ..ElectrsStatus::default()
            },
        );

        assert!(!app.bitcoin_ready());
        assert!(!app.electrs_status.connected);
        assert!(!app.electrs_status.ready);
        assert!(app.electrs_status.bitcoin_error.is_some());
        assert!(app.electrs_status.connect_error.is_some());
        assert!(app.electrs_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .any(|line| line.contains("Electrs Bitcoin check failed"))
        }));

        apply_current_status(
            &mut app,
            healthy_bitcoin_probe(blockchain_info(100, 100, false, false)),
            ready_electrs_status(),
        );

        assert!(app.bitcoin_ready());
        assert!(app.electrs_status.connected);
        assert!(app.electrs_status.ready);
        assert!(app.electrs_status.bitcoin_error.is_none());
        assert!(app.electrs_status.connect_error.is_none());
        assert!(app.electrs_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .any(|line| line.contains("connectivity check recovered"))
                && lines
                    .iter()
                    .any(|line| line.contains("ready to serve BitEngine clients"))
        }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn bitcoin_exit_invalidates_electrs_and_blocks_a_mixed_generation_relaunch(
    ) -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let bitcoin_process_id = wait_for_pids(&bitcoin_pid_log, 1)?[0];
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let _ = wait_for_pids(&electrs_pid_log, 1)?;
        app.electrs_status = ready_electrs_status();
        let stale_identity = StatusPollIdentity {
            request_id: 71,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(stale_identity);

        // SAFETY: the PID belongs to the helper process launched above.
        assert_eq!(unsafe { libc::kill(bitcoin_process_id, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.bitcoin_handle.is_some() && Instant::now() < deadline {
            app.reconcile_node_lifecycle();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(app.bitcoin_handle.is_none());
        assert!(app.electrs_handle.is_some());
        assert!(!app.electrs_status.connected);
        assert!(!app.electrs_status.ready);
        assert!(app
            .electrs_status
            .bitcoin_error
            .as_deref()
            .is_some_and(|error| error.contains("stop Electrs")));

        app.apply_status_poll(StatusPollResult {
            identity: stale_identity,
            bitcoin_probe: healthy_bitcoin_probe(blockchain_info(100, 100, false, false)),
            electrs_status: ready_electrs_status(),
        });
        assert!(!app.electrs_status.connected);
        assert!(!app.electrs_status.ready);

        drop(app.launch_bitcoin());
        assert_eq!(wait_for_pids(&bitcoin_pid_log, 1)?.len(), 1);
        assert!(app.overlay_message.as_deref().is_some_and(|message| {
            message.contains("previous Bitcoin generation") && message.contains("Stop Electrs")
        }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn stale_prelaunch_poll_cannot_clear_a_new_electrs_generation() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        mark_bitcoin_dependency_ready(&mut app)?;

        let stale_identity = StatusPollIdentity {
            request_id: 17,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(stale_identity);
        drop(app.launch_electrs());
        let _ = wait_for_pids(&electrs_pid_log, 1)?;

        app.apply_status_poll(StatusPollResult {
            identity: stale_identity,
            bitcoin_probe: failed_bitcoin_probe("stale poll"),
            electrs_status: ElectrsStatus::default(),
        });

        assert!(app.electrs_handle.is_some());
        assert!(app.electrs_status.running);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn occupied_electrs_handle_blocks_duplicate_launch_despite_stale_status() -> anyhow::Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let pids = wait_for_pids(&electrs_pid_log, 1)?;

        app.electrs_status = ElectrsStatus::default();
        drop(app.launch_electrs());
        std::thread::sleep(Duration::from_millis(100));

        assert_eq!(wait_for_pids(&electrs_pid_log, 1)?.len(), 1);
        assert_eq!(pids.len(), 1);
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("already running")));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unexpected_electrs_exit_resets_all_runtime_status() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let process_id = wait_for_pids(&electrs_pid_log, 1)?[0];
        app.electrs_status = ElectrsStatus {
            running: true,
            connected: true,
            synced: true,
            ready: true,
            electrs_height: Some(100),
            bitcoin_blocks: Some(100),
            bitcoin_headers: Some(100),
            sync_percent: Some(100.0),
            metrics_error: Some("stale metrics".to_owned()),
            bitcoin_error: Some("stale Bitcoin".to_owned()),
            connect_error: Some("stale connection".to_owned()),
        };

        // SAFETY: the PID belongs to the helper process launched above.
        assert_eq!(unsafe { libc::kill(process_id, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.electrs_handle.is_some() && Instant::now() < deadline {
            app.reconcile_node_lifecycle();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(app.electrs_handle.is_none());
        assert_eq!(app.electrs_status, ElectrsStatus::default());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rpc_failure_clears_readiness_for_the_current_bitcoin_process() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        app.bitcoin_synced = true;
        app.block_height = 100;
        let identity = StatusPollIdentity {
            request_id: 23,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(identity);

        app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe: failed_bitcoin_probe("RPC timed out"),
            electrs_status: ElectrsStatus::default(),
        });

        assert!(!app.bitcoin_synced);
        assert_eq!(app.block_height, 0);
        assert!(app.bitcoin_running);
        Ok(())
    }

    #[test]
    fn successful_poll_without_an_owned_process_cannot_create_readiness() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let identity = StatusPollIdentity {
            request_id: 29,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(identity);

        app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe: healthy_bitcoin_probe(blockchain_info(100, 100, false, false)),
            electrs_status: ElectrsStatus {
                running: true,
                connected: true,
                ready: true,
                ..ElectrsStatus::default()
            },
        });

        assert!(!app.bitcoin_running);
        assert!(!app.bitcoin_synced);
        assert_eq!(app.block_height, 0);
        assert_eq!(app.electrs_status, ElectrsStatus::default());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn active_node_locks_path_mutation_and_activation() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        let original_config = app.config.binaries_path.clone();

        assert!(!app.paths_are_editable());
        drop(app.save_paths());
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("node is running")));

        app.pending_path_save = Some(31);
        drop(app.apply_paths_saved(31, Ok(alternate_config(temporary.path()))));
        assert_eq!(app.config.binaries_path, original_config);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn active_lifecycle_retains_the_bitcoin_endpoint_snapshot() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let bitcoin_data = temporary.path().join("BitcoinChain");
        std::fs::create_dir(&bitcoin_data)?;
        std::fs::write(
            bitcoin_data.join("bitcoin.conf"),
            "rpcport=18443\nport=18444\nbind=127.0.0.1\n",
        )?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let _ = wait_for_pids(&bitcoin_pid_log, 1)?;
        let endpoints = app
            .managed_endpoints
            .as_ref()
            .context("managed endpoint snapshot")?;
        assert_eq!(endpoints.rpc_port, 18_443);
        assert_eq!(
            endpoints.p2p_candidates.first().copied(),
            Some(SocketAddr::from(([127, 0, 0, 1], 18_444)))
        );

        std::fs::write(
            bitcoin_data.join("bitcoin.conf"),
            "rpcport=19999\nport=20000\nbind=[::1]\n",
        )?;
        std::fs::write(bitcoin_data.join(".cookie"), "managed:secret\n")?;

        mark_bitcoin_dependency_ready(&mut app)?;
        assert_eq!(
            app.bitcoin_rpc_auth()
                .map_err(anyhow::Error::msg)?
                .endpoint
                .port(),
            18_443
        );
        drop(app.launch_electrs());
        let _ = wait_for_pids(&electrs_pid_log, 1)?;
        assert!(app
            .electrs_queue
            .lock()
            .is_ok_and(|lines| lines.iter().any(|line| {
                line.contains("--daemon-rpc-addr 127.0.0.1:18443")
                    && line.contains("--daemon-p2p-addr 127.0.0.1:18444")
            })));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dropping_app_terminates_its_managed_node() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let process_id = wait_for_pids(&bitcoin_pid_log, 1)?[0];

        drop(app);

        assert_process_exits(process_id);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dropping_app_force_terminates_two_unresponsive_nodes_within_a_tight_bound(
    ) -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_stubborn_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_stubborn_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let bitcoin_process_id = wait_for_pids(&bitcoin_pid_log, 1)?[0];
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let electrs_process_id = wait_for_pids(&electrs_pid_log, 1)?[0];
        let started = Instant::now();

        drop(app);

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_process_exits(bitcoin_process_id);
        assert_process_exits(electrs_process_id);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dropping_app_interrupts_shutdown_workers_and_clears_runtime_state() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        let electrs_pid_log = temporary.path().join("electrs-pids");
        install_stubborn_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        install_stubborn_node_helper(temporary.path(), "electrs", &electrs_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let bitcoin_process_id = wait_for_pids(&bitcoin_pid_log, 1)?[0];
        mark_bitcoin_dependency_ready(&mut app)?;
        drop(app.launch_electrs());
        let electrs_process_id = wait_for_pids(&electrs_pid_log, 1)?[0];
        app.bitcoin_synced = true;
        app.block_height = 123;

        drop(app.shutdown_both());

        assert!(app.bitcoin_shutdown.is_some());
        assert!(app.electrs_shutdown.is_some());
        assert!(!app.bitcoin_synced);
        assert_eq!(app.block_height, 0);
        let started = Instant::now();
        drop(app);

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_process_exits(bitcoin_process_id);
        assert_process_exits(electrs_process_id);
        Ok(())
    }

    #[test]
    fn dropping_app_forces_and_joins_shutdown_workers() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let completed = Arc::new(AtomicUsize::new(0));
        let worker_completed = Arc::clone(&completed);
        let mut app = test_app(temporary.path());
        app.bitcoin_shutdown = Some(ShutdownWorker::spawn(
            "bitengine-test-shutdown",
            move |force| {
                while !force.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                worker_completed.store(1, Ordering::Release);
            },
        )?);

        drop(app.launch_bitcoin());
        assert!(app
            .overlay_message
            .as_deref()
            .is_some_and(|message| message.contains("still shutting down")));

        drop(app);

        assert_eq!(completed.load(Ordering::Acquire), 1);
        Ok(())
    }
}
