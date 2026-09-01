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
//! The UI drains those queues on every `OutputTick` (500 ms timer).
//! RPC polling happens on every `RpcTick` (5 s timer) via an async `Task`.
//!
//! This keeps the UI thread non-blocking at all times.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[path = "ui_render.rs"]
mod ui_render;

use iced::{
    keyboard, time,
    widget::{self, scrollable},
    Element, Subscription, Task,
};
use tokio::sync::{mpsc, oneshot};

use crate::{
    binaries::{
        self, AvailableVersions, BinaryKind, BuildEvent, BuildFailure, BuildOperationId,
        BuildRequest, BuildService, BuildStage, BuildSummary, DependencyInstallOutcome,
        DependencyReport, InstalledVersions, PersistedBuild, PersistedBuildStatus, ReleaseVersion,
    },
    bitcoin_config::{resolve_managed_endpoints, ManagedBitcoinEndpoints},
    bitcoin_status,
    config::{BuildPerformance, BuildSettings, Config, ThemePreference},
    connection::{
        self, BitcoinReadiness, ConnectionReadiness, ElectrsBindPolicy, ElectrsListenAddr,
        ElectrsReadiness, ElectrumEndpoint, LocalEndpointState, DEFAULT_ELECTRUM_PORT,
    },
    electrs_status::{self, ElectrsStatus},
    platform::{self, Platform},
    process_manager::{self, new_queue, ElectrsBitcoinConnection, OutputQueue, ProcessHandle},
    rpc::{self, BlockchainInfo, NetworkInfo, RpcAuth},
    tor::{ElectrsTorTarget, TorManager, TorManagerConfig, TorStatus, ELECTRUM_ONION_PORT},
};

const BUILD_EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_BUILD_EVENTS_PER_TICK: usize = 256;
const BITCOIN_RPC_STARTUP_TIMEOUT: Duration = Duration::from_mins(5);
const BITCOIN_RPC_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_BOTTOM_TOLERANCE: f32 = 2.0;
const OUTPUT_PAGE_SCROLL_FRACTION: f32 = 0.9;
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PENDING_PATH_SAVE_SETTINGS_MESSAGE: &str =
    "Wait for the pending path save to finish before changing settings.";

// ── Message ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
#[expect(
    private_interfaces,
    reason = "Iced requires the application message type to be public, while Tor command payloads remain an internal implementation detail"
)]
pub enum Message {
    // ── Timer ticks ──────────────────────────────────────────────────────────
    /// 500 ms — drain process output queues into terminal buffers.
    OutputTick,
    /// 5 s — poll Bitcoin RPC for chain state.
    RpcTick,

    // ── Appearance and connection preferences ───────────────────────────────
    SystemThemeChanged(iced::theme::Mode),
    ThemePreferenceChanged(ThemePreference),
    ConnectionModeChanged(ConnectionMode),
    LocalNetworkAccessChanged(bool),
    TorEnabledChanged(bool),
    TorAutoStartChanged(bool),
    StartTor,
    CopySelectedEndpoint,
    RetryTor,
    LanAddressDiscovered(Result<IpAddr, String>),
    TorManagerStarted(Result<TorManager, String>),
    TorCommandFinished {
        operation: TorOperation,
        result: Result<(), String>,
    },
    TorElectrsStateFinished {
        request_id: u64,
        state: (ElectrsTorTarget, bool),
        result: Result<(), String>,
    },
    WindowCloseRequested(iced::window::Id),
    TorShutdownFinished {
        window_id: iced::window::Id,
        result: Result<(), String>,
    },

    // ── Terminal viewport scrolling ─────────────────────────────────────────
    OutputViewportChanged {
        pane: OutputPane,
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    },
    OutputPaneHoverChanged {
        pane: OutputPane,
        hovered: bool,
    },
    OutputFollowLatest(OutputPane),
    OutputPageUp,
    OutputPageDown,

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
    InstallationRecoveryFinished {
        request_id: u64,
        destination: PathBuf,
        result: Result<(), String>,
    },
    RetryInstallationRecovery,
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
    BuildPerformanceChanged(BuildPerformance),
    KeepSourceChanged(bool),
    CleanBuildChanged(bool),
    VerboseBuildOutputChanged(bool),
    RestoreBuildDefaults,
    CheckDependencies,
    DependenciesScanned {
        request_id: u64,
        report: DependencyReport,
    },
    InstallDependencies,
    DependenciesInstalled {
        request_id: u64,
        outcome: DependencyInstallOutcome,
    },
    ToggleDependencyDetails,

    // ── Async results ─────────────────────────────────────────────────────────
    StatusPollReceived(Box<StatusPollResult>),

    // ── Modal / overlay ───────────────────────────────────────────────────────
    /// Dismiss the info/error overlay.
    DismissOverlay,
}

#[derive(Clone, Copy)]
enum PrefMsg {
    SystemTheme(iced::theme::Mode),
    Theme(ThemePreference),
    ConnectionMode(ConnectionMode),
    TorAutoStart(bool),
}

enum TorMsg {
    Enabled(bool),
    Start,
    CopyEndpoint,
    Retry,
    LanAddress(Result<IpAddr, String>),
    ManagerStarted(Result<TorManager, String>),
}

#[derive(Clone, Copy)]
enum OutMsg {
    PaneHoverChanged { pane: OutputPane, hovered: bool },
    FollowLatest(OutputPane),
    PageUp,
    PageDown,
}

enum PathMsg {
    BinariesChanged(String),
    BitcoinDataChanged(String),
    ElectrsDataChanged(String),
    BrowseBinaries,
    BrowseBitcoinData,
    BrowseElectrsData,
    BinariesBrowsed(Option<String>),
    BitcoinDataBrowsed(Option<String>),
    ElectrsDataBrowsed(Option<String>),
    TogglePanel,
}

#[derive(Clone, Copy)]
enum NavMsg {
    OpenDashboard,
    OpenBinaries,
    RefreshBinaryInfo,
}

#[derive(Clone, Copy)]
enum BuildMsg {
    Cancel,
    ToggleDetails,
    ToggleAdvanced,
}

#[derive(Clone, Copy)]
enum DepMsg {
    Check,
    Install,
    ToggleDetails,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Dashboard,
    Binaries,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionMode {
    #[default]
    Local,
    Tor,
}

impl ConnectionMode {
    pub const ALL: [Self; 2] = [Self::Local, Self::Tor];
}

impl std::fmt::Display for ConnectionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "Local",
            Self::Tor => "Tor",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorOperation {
    Enable,
    Disable,
    Retry,
}

impl TorOperation {
    const fn runtime_state(self) -> Option<bool> {
        match self {
            Self::Enable => Some(true),
            Self::Disable => Some(false),
            Self::Retry => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputPane {
    Bitcoin,
    Electrs,
    Build,
}

#[derive(Debug, Clone, Copy)]
struct OutputViewportState {
    follow_output: bool,
    offset_y: f32,
    viewport_height: f32,
    content_height: f32,
}

impl Default for OutputViewportState {
    fn default() -> Self {
        Self {
            follow_output: true,
            offset_y: 0.0,
            viewport_height: 0.0,
            content_height: 0.0,
        }
    }
}

impl OutputViewportState {
    fn update(&mut self, offset_y: f32, viewport_height: f32, content_height: f32) {
        let offset_y = finite_nonnegative(offset_y);
        let viewport_height = finite_nonnegative(viewport_height);
        let content_height = finite_nonnegative(content_height);
        let maximum_offset = (content_height - viewport_height).max(0.0);
        let offset_y = offset_y.min(maximum_offset);
        let moved_up = offset_y + f32::EPSILON < self.offset_y;
        let content_grew = content_height > self.content_height + f32::EPSILON;
        let at_bottom = maximum_offset - offset_y <= OUTPUT_BOTTOM_TOLERANCE;

        self.follow_output = if at_bottom {
            true
        } else if moved_up {
            false
        } else if content_grew {
            self.follow_output
        } else {
            false
        };
        self.offset_y = offset_y;
        self.viewport_height = viewport_height;
        self.content_height = content_height;
    }

    fn can_scroll(&self) -> bool {
        self.content_height > self.viewport_height + f32::EPSILON
    }

    fn page_scroll_distance(&self) -> f32 {
        self.viewport_height * OUTPUT_PAGE_SCROLL_FRACTION
    }
}

#[derive(Debug, Default)]
struct OutputViewports {
    bitcoin: OutputViewportState,
    electrs: OutputViewportState,
    build: OutputViewportState,
}

impl OutputViewports {
    const fn get(&self, pane: OutputPane) -> &OutputViewportState {
        match pane {
            OutputPane::Bitcoin => &self.bitcoin,
            OutputPane::Electrs => &self.electrs,
            OutputPane::Build => &self.build,
        }
    }

    const fn get_mut(&mut self, pane: OutputPane) -> &mut OutputViewportState {
        match pane {
            OutputPane::Bitcoin => &mut self.bitcoin,
            OutputPane::Electrs => &mut self.electrs,
            OutputPane::Build => &mut self.build,
        }
    }
}

const fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
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
    dependency_report: Option<DependencyReport>,
    dependency_load: DependencyLoad,
    dependency_request: Option<u64>,
    dependency_message: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyLoad {
    Idle,
    Checking,
    Installing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallationRecoveryState {
    Checking {
        request_id: u64,
        destination: PathBuf,
    },
    Ready {
        destination: PathBuf,
    },
    Failed {
        destination: PathBuf,
        error: String,
    },
}

impl InstallationRecoveryState {
    fn is_ready_for(&self, destination: &Path) -> bool {
        matches!(
            self,
            Self::Ready {
                destination: ready_destination
            } if ready_destination == destination
        )
    }
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
    dependency_details: bool,
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
            dependency_report: None,
            dependency_load: DependencyLoad::Idle,
            dependency_request: None,
            dependency_message: None,
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

#[derive(Debug)]
struct BitcoinRpcStartup {
    deadline: Instant,
    last_status: Option<String>,
    last_diagnostic: Option<String>,
}

#[derive(Debug)]
struct ConnectionQr {
    payload: String,
    width: usize,
    cells: Vec<bool>,
}

impl ConnectionQr {
    fn encode(payload: String) -> qrcode::types::QrResult<Self> {
        let encoded = qrcode::QrCode::new(payload.as_bytes())?;
        let width = encoded.width();
        let cells = encoded
            .into_colors()
            .into_iter()
            .map(|cell| cell == qrcode::Color::Dark)
            .collect();

        Ok(Self {
            payload,
            width,
            cells,
        })
    }
}

impl BitcoinRpcStartup {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + BITCOIN_RPC_STARTUP_TIMEOUT,
            last_status: None,
            last_diagnostic: None,
        }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "UI process, synchronization, and independent RPC/P2P readiness facts must remain separately observable"
)]
pub struct App {
    // ── Config ───────────────────────────────────────────────────────────────
    config: Config,
    #[cfg(test)]
    config_save_override: Option<fn(&Config) -> anyhow::Result<()>>,

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
    bitcoin_rpc_startup: Option<BitcoinRpcStartup>,
    bitcoin_rpc_startup_status: Option<String>,
    bitcoin_p2p_error: Option<String>,
    bitcoin_compatibility_error: Option<String>,
    bitcoin_process_error: Option<String>,
    bitcoin_blockchain_info: Option<BlockchainInfo>,
    electrs_status: ElectrsStatus,
    electrs_process_error: Option<String>,
    active_electrs_listener: Option<ElectrsListenAddr>,
    electrs_listener_invalidation: Option<String>,
    lan_address: Option<Result<IpAddr, String>>,
    electrs_launch_pending_for_lan: bool,
    block_height: u64,

    // ── UI state ──────────────────────────────────────────────────────────────
    page: Page,
    system_theme: iced::theme::Mode,
    selected_connection_mode: ConnectionMode,
    copied_endpoint_at: Option<Instant>,
    connection_endpoint: Option<ElectrumEndpoint>,
    connection_endpoint_payload: Option<String>,
    connection_qr: Option<ConnectionQr>,
    connection_qr_error: Option<String>,
    tor_manager: Option<TorManager>,
    tor_status_subscription: Option<tokio::sync::watch::Receiver<TorStatus>>,
    tor_status: TorStatus,
    tor_manager_starting: bool,
    tor_runtime_requested: bool,
    tor_runtime_command_in_flight: Option<bool>,
    tor_control_error: Option<String>,
    tor_electrs_sync_error: Option<String>,
    tor_forwarded_electrs_state: Option<(ElectrsTorTarget, bool)>,
    tor_failed_electrs_state: Option<(ElectrsTorTarget, bool)>,
    tor_latest_sync_request: Option<(u64, (ElectrsTorTarget, bool))>,
    next_tor_sync_request: u64,
    closing: bool,
    paths_visible: bool,
    binary_page: BinaryPageState,
    output_viewports: OutputViewports,
    hovered_output_pane: Option<OutputPane>,
    build_service: BuildService,
    build_event_tx: mpsc::Sender<BuildEvent>,
    build_event_rx: mpsc::Receiver<BuildEvent>,
    installation_recovery: InstallationRecoveryState,
    next_installation_recovery: u64,
    pending_path_save: Option<u64>,
    next_path_save: u64,
    next_inventory_request: u64,
    next_build_operation: u64,
    next_dependency_request: u64,
    lifecycle_generation: u64,
    next_status_poll: u64,
    active_status_poll: Option<StatusPollIdentity>,

    /// Non-empty ⇒ display an overlay dialog with this message.
    overlay_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StatusPollResult {
    identity: StatusPollIdentity,
    bitcoin_probe: Option<BitcoinProbeResult>,
    electrs_status: ElectrsStatus,
    electrs_listener_validation: ElectrsListenerValidation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ElectrsListenerValidation {
    NotRequired,
    Valid,
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ElectrsListenerPollPlan {
    listener_for_probe: Option<ElectrsListenAddr>,
    validation: ElectrsListenerValidation,
}

#[derive(Debug, Clone)]
struct BitcoinProbeResult {
    blockchain_info: Result<BlockchainInfo, String>,
    network_info: Result<NetworkInfo, String>,
    rpc_addr: Option<SocketAddr>,
    rpc_readiness: ManagedRpcReadiness,
    p2p_result: Result<SocketAddr, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagedRpcReadiness {
    Ready,
    Warmup { status: String, diagnostic: String },
    Unavailable { diagnostic: String },
    Fatal { diagnostic: String },
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

    pub fn initial_task(&self) -> Task<Message> {
        let initially_enabled = self.tor_runtime_requested;
        let config_file = Config::config_file_path();
        let start_tor_manager = Task::perform(
            async move {
                let target = ElectrsTorTarget::new(SocketAddr::from((
                    Ipv4Addr::LOCALHOST,
                    ELECTRUM_ONION_PORT,
                )))
                .map_err(|error| error.to_string())?;
                let config = TorManagerConfig::for_bitengine_config(&config_file, target)
                    .map_err(|error| error.to_string())?
                    .initially_enabled(initially_enabled)
                    .initially_electrs_ready(false);
                TorManager::spawn(config).map_err(|error| error.to_string())
            },
            Message::TorManagerStarted,
        );
        let check_installation = match &self.installation_recovery {
            InstallationRecoveryState::Checking {
                request_id,
                destination: _,
            } => installation_recovery_task(*request_id, self.config.clone()),
            InstallationRecoveryState::Ready { .. } | InstallationRecoveryState::Failed { .. } => {
                Task::none()
            }
        };
        Task::batch([start_tor_manager, check_installation])
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
        let installation_destination = config.binaries_path.clone();
        let tor_runtime_requested = config.tor_enabled && config.tor_auto_start;
        let build_service = BuildService::new(build_state_path);
        let binary_page = BinaryPageState::new(build_service.recovered());

        log_startup(&config, config_warning, &bitcoin_queue, &electrs_queue);

        Self {
            config,
            #[cfg(test)]
            config_save_override: None,
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
            bitcoin_rpc_startup: None,
            bitcoin_rpc_startup_status: None,
            bitcoin_p2p_error: None,
            bitcoin_compatibility_error: None,
            bitcoin_process_error: None,
            bitcoin_blockchain_info: None,
            electrs_status: ElectrsStatus::default(),
            electrs_process_error: None,
            active_electrs_listener: None,
            electrs_listener_invalidation: None,
            lan_address: None,
            electrs_launch_pending_for_lan: false,
            block_height: 0,
            page: Page::Dashboard,
            system_theme: iced::theme::Mode::Light,
            selected_connection_mode: ConnectionMode::default(),
            copied_endpoint_at: None,
            connection_endpoint: None,
            connection_endpoint_payload: None,
            connection_qr: None,
            connection_qr_error: None,
            tor_manager: None,
            tor_status_subscription: None,
            tor_status: TorStatus::Disabled,
            tor_manager_starting: true,
            tor_runtime_requested,
            tor_runtime_command_in_flight: None,
            tor_control_error: None,
            tor_electrs_sync_error: None,
            tor_forwarded_electrs_state: None,
            tor_failed_electrs_state: None,
            tor_latest_sync_request: None,
            next_tor_sync_request: 1,
            closing: false,
            paths_visible: false,
            binary_page,
            output_viewports: OutputViewports::default(),
            hovered_output_pane: None,
            build_service,
            build_event_tx,
            build_event_rx,
            installation_recovery: InstallationRecoveryState::Checking {
                request_id: 1,
                destination: installation_destination,
            },
            next_installation_recovery: 2,
            pending_path_save: None,
            next_path_save: 1,
            next_inventory_request: 1,
            next_build_operation: 1,
            next_dependency_request: 1,
            lifecycle_generation: 1,
            next_status_poll: 1,
            active_status_poll: None,
            overlay_message: None,
        }
    }

    // ── update ────────────────────────────────────────────────────────────────

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = match message {
            Message::OutputTick => self.handle_output_tick(),
            Message::RpcTick => self.handle_rpc_tick(),
            Message::SystemThemeChanged(mode) => self.pref(PrefMsg::SystemTheme(mode)),
            Message::ThemePreferenceChanged(preference) => self.pref(PrefMsg::Theme(preference)),
            Message::ConnectionModeChanged(mode) => self.pref(PrefMsg::ConnectionMode(mode)),
            Message::LocalNetworkAccessChanged(enabled) => {
                self.update_local_network_access(enabled)
            }
            Message::TorEnabledChanged(enabled) => self.tor(TorMsg::Enabled(enabled)),
            Message::TorAutoStartChanged(enabled) => self.pref(PrefMsg::TorAutoStart(enabled)),
            Message::StartTor => self.tor(TorMsg::Start),
            Message::CopySelectedEndpoint => self.tor(TorMsg::CopyEndpoint),
            Message::RetryTor => self.tor(TorMsg::Retry),
            Message::LanAddressDiscovered(result) => self.tor(TorMsg::LanAddress(result)),
            Message::TorManagerStarted(result) => self.tor(TorMsg::ManagerStarted(result)),
            Message::TorCommandFinished { operation, result } => {
                self.apply_tor_command_finished(operation, result)
            }
            Message::TorElectrsStateFinished {
                request_id,
                state,
                result,
            } => self.apply_tor_electrs_state_finished(request_id, state, result),
            Message::WindowCloseRequested(window_id) => self.close_window(window_id),
            Message::TorShutdownFinished { window_id, result } => {
                self.apply_tor_shutdown_finished(window_id, result)
            }
            Message::OutputViewportChanged {
                pane,
                offset_y,
                viewport_height,
                content_height,
            } => self.update_output_viewport_state(pane, offset_y, viewport_height, content_height),
            Message::OutputPaneHoverChanged { pane, hovered } => {
                self.output(OutMsg::PaneHoverChanged { pane, hovered })
            }
            Message::OutputFollowLatest(pane) => self.output(OutMsg::FollowLatest(pane)),
            Message::OutputPageUp => self.output(OutMsg::PageUp),
            Message::OutputPageDown => self.output(OutMsg::PageDown),
            Message::BinariesPathChanged(path) => self.path(PathMsg::BinariesChanged(path)),
            Message::BitcoinDataPathChanged(path) => self.path(PathMsg::BitcoinDataChanged(path)),
            Message::ElectrsDataPathChanged(path) => self.path(PathMsg::ElectrsDataChanged(path)),
            Message::BrowseBinaries => self.path(PathMsg::BrowseBinaries),
            Message::BrowseBitcoinData => self.path(PathMsg::BrowseBitcoinData),
            Message::BrowseElectrsData => self.path(PathMsg::BrowseElectrsData),
            Message::BinariesBrowsed(path) => self.path(PathMsg::BinariesBrowsed(path)),
            Message::BitcoinDataBrowsed(path) => self.path(PathMsg::BitcoinDataBrowsed(path)),
            Message::ElectrsDataBrowsed(path) => self.path(PathMsg::ElectrsDataBrowsed(path)),
            Message::SavePaths => self.save_paths(),
            Message::PathsSaved { request_id, result } => {
                self.apply_paths_saved(request_id, result)
            }
            Message::InstallationRecoveryFinished {
                request_id,
                destination,
                result,
            } => self.apply_installation_recovery_finished(request_id, destination, result),
            Message::RetryInstallationRecovery => self.retry_installation_recovery(),
            Message::TogglePathsPanel => self.path(PathMsg::TogglePanel),
            Message::LaunchBitcoin => self.launch_bitcoin(),
            Message::LaunchElectrs => self.launch_electrs(),
            Message::ShutdownBoth => self.shutdown_both(),
            Message::ShutdownElectrsOnly => self.shutdown_electrs_only(),
            message @ (Message::OpenDashboard
            | Message::OpenBinaries
            | Message::RefreshBinaryInfo
            | Message::InstalledVersionsLoaded { .. }
            | Message::AvailableVersionsLoaded { .. }
            | Message::SelectBitcoinVersion(_)
            | Message::SelectElectrsVersion(_)
            | Message::StartBuild(_)
            | Message::BuildFinished { .. }
            | Message::CancelBuild
            | Message::ToggleBuildDetails
            | Message::ToggleBuildAdvanced
            | Message::BuildPerformanceChanged(_)
            | Message::KeepSourceChanged(_)
            | Message::CleanBuildChanged(_)
            | Message::VerboseBuildOutputChanged(_)
            | Message::RestoreBuildDefaults
            | Message::CheckDependencies
            | Message::DependenciesScanned { .. }
            | Message::InstallDependencies
            | Message::DependenciesInstalled { .. }
            | Message::ToggleDependencyDetails) => self.handle_binary_message(message),
            Message::StatusPollReceived(result) => self.apply_status_poll(*result),
            Message::DismissOverlay => {
                self.overlay_message = None;
                Task::none()
            }
        };
        let mut tasks = vec![task];
        tasks.extend(self.sync_tor_manager());
        self.refresh_connection_qr();
        Task::batch(tasks)
    }

    fn pref(&mut self, message: PrefMsg) -> Task<Message> {
        match message {
            PrefMsg::SystemTheme(mode) => {
                self.system_theme = mode;
                Task::none()
            }
            PrefMsg::Theme(preference) => self
                .update_ui_preferences("theme", move |config| config.theme_preference = preference),
            PrefMsg::ConnectionMode(mode) => {
                self.selected_connection_mode = mode;
                self.copied_endpoint_at = None;
                Task::none()
            }
            PrefMsg::TorAutoStart(enabled) => {
                self.update_ui_preferences("Tor automatic startup", move |config| {
                    config.tor_auto_start = enabled;
                })
            }
        }
    }

    fn update_local_network_access(&mut self, enabled: bool) -> Task<Message> {
        if self.electrs_handle.is_some() || self.electrs_shutdown.is_some() {
            self.overlay_message = Some(
                "Local network access cannot change while electrs is running or shutting down. Stop electrs first; the setting takes effect on its next launch."
                    .to_owned(),
            );
            return Task::none();
        }

        let previous = self.config.local_network_access;
        let task = self.update_ui_preferences("local network access", move |config| {
            config.local_network_access = enabled;
        });
        if self.config.local_network_access != previous {
            self.lan_address = None;
        }
        task
    }

    fn tor(&mut self, message: TorMsg) -> Task<Message> {
        match message {
            TorMsg::Enabled(enabled) => self.update_tor_enabled(enabled),
            TorMsg::Start => self.start_tor(),
            TorMsg::CopyEndpoint => self.copy_selected_endpoint(),
            TorMsg::Retry => self.retry_tor(),
            TorMsg::LanAddress(result) => self.apply_lan_address(result),
            TorMsg::ManagerStarted(result) => self.apply_tor_manager_started(result),
        }
    }

    fn update_tor_enabled(&mut self, enabled: bool) -> Task<Message> {
        if enabled && self.tor_manager.is_none() && !self.tor_manager_starting {
            self.tor_control_error = Some(
                "The embedded Tor manager is unavailable; restart BitEngine before enabling Tor."
                    .to_owned(),
            );
            return Task::none();
        }

        let previous_runtime_request = self.tor_runtime_requested;
        let persist = self.update_ui_preferences("Tor access", move |config| {
            config.tor_enabled = enabled;
        });
        if self.config.tor_enabled == enabled {
            self.tor_runtime_requested = enabled;
            self.tor_control_error = None;
            if enabled {
                self.tor_status = TorStatus::Starting;
            }
            let runtime = self.schedule_tor_runtime_command(enabled);
            Task::batch([persist, runtime])
        } else {
            self.tor_runtime_requested = previous_runtime_request;
            persist
        }
    }

    fn start_tor(&mut self) -> Task<Message> {
        if !self.config.tor_enabled {
            Task::none()
        } else if self.tor_manager.is_some() {
            self.tor_runtime_requested = true;
            self.tor_control_error = None;
            self.tor_status = TorStatus::Starting;
            self.schedule_tor_runtime_command(true)
        } else {
            self.tor_control_error = Some(
                "The embedded Tor manager is not available yet. Try again in a moment.".to_owned(),
            );
            Task::none()
        }
    }

    fn copy_selected_endpoint(&mut self) -> Task<Message> {
        self.selected_connection_endpoint()
            .map_or_else(Task::none, |endpoint| {
                self.copied_endpoint_at = Some(Instant::now());
                iced::clipboard::write(endpoint.payload())
            })
    }

    fn retry_tor(&mut self) -> Task<Message> {
        if !self.config.tor_enabled {
            Task::none()
        } else if let Some(manager) = self.tor_manager.clone() {
            self.tor_runtime_requested = true;
            self.tor_control_error = None;
            self.tor_status = TorStatus::Starting;
            retry_tor_task(manager)
        } else {
            self.tor_control_error = Some(
                "The embedded Tor manager is unavailable; restart BitEngine to recreate it."
                    .to_owned(),
            );
            Task::none()
        }
    }

    fn apply_lan_address(&mut self, result: Result<IpAddr, String>) -> Task<Message> {
        self.lan_address = Some(result.clone());
        if !self.electrs_launch_pending_for_lan {
            return Task::none();
        }
        self.electrs_launch_pending_for_lan = false;
        match result {
            Ok(_) => Task::done(Message::LaunchElectrs),
            Err(error) => {
                self.overlay_message = Some(format!(
                    "Local network access could not be prepared:\n{error}\n\nNo electrs listener was started."
                ));
                Task::none()
            }
        }
    }

    fn apply_tor_manager_started(&mut self, result: Result<TorManager, String>) -> Task<Message> {
        self.tor_manager_starting = false;
        self.tor_runtime_command_in_flight = None;
        match result {
            Ok(manager) => {
                let status = manager.status();
                let reconcile_enabled =
                    tor_runtime_reconciliation(&status, self.tor_runtime_requested);
                self.tor_status = reconcile_enabled.map_or(status, |enabled| {
                    if enabled {
                        TorStatus::Starting
                    } else {
                        TorStatus::Disabled
                    }
                });
                self.tor_status_subscription = Some(manager.subscribe());
                self.tor_manager = Some(manager);
                self.tor_control_error = None;
                self.tor_electrs_sync_error = None;
                self.tor_forwarded_electrs_state = None;
                self.tor_failed_electrs_state = None;
                self.tor_latest_sync_request = None;
                reconcile_enabled.map_or_else(Task::none, |enabled| {
                    self.schedule_tor_runtime_command(enabled)
                })
            }
            Err(error) => {
                self.tor_manager = None;
                self.tor_status_subscription = None;
                self.tor_forwarded_electrs_state = None;
                self.tor_failed_electrs_state = None;
                self.tor_latest_sync_request = None;
                self.tor_electrs_sync_error = None;
                self.tor_status = TorStatus::Error {
                    message: error.clone(),
                    retryable: false,
                };
                self.tor_control_error = Some(error);
                Task::none()
            }
        }
    }

    fn apply_tor_command_finished(
        &mut self,
        operation: TorOperation,
        result: Result<(), String>,
    ) -> Task<Message> {
        if let Some(completed_state) = operation.runtime_state() {
            return self.finish_tor_runtime_command(completed_state, result);
        }
        match result {
            Ok(()) => {
                self.tor_control_error = None;
                self.tor_forwarded_electrs_state = None;
                self.tor_failed_electrs_state = None;
                self.tor_electrs_sync_error = None;
            }
            Err(error) => self.tor_control_error = Some(error),
        }
        Task::none()
    }

    fn apply_tor_electrs_state_finished(
        &mut self,
        request_id: u64,
        state: (ElectrsTorTarget, bool),
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.tor_latest_sync_request == Some((request_id, state)) {
            self.tor_latest_sync_request = None;
            match result {
                Ok(()) => {
                    self.tor_forwarded_electrs_state = Some(state);
                    self.tor_failed_electrs_state = None;
                    self.tor_electrs_sync_error = None;
                }
                Err(error) => {
                    self.tor_forwarded_electrs_state = None;
                    self.tor_failed_electrs_state = Some(state);
                    self.tor_electrs_sync_error = Some(error);
                }
            }
        }
        Task::none()
    }

    fn close_window(&mut self, window_id: iced::window::Id) -> Task<Message> {
        if self.closing {
            Task::none()
        } else {
            self.closing = true;
            self.tor_status_subscription = None;
            self.tor_manager.take().map_or_else(
                || iced::window::close(window_id),
                |manager| shutdown_tor_task(manager, window_id),
            )
        }
    }

    fn apply_tor_shutdown_finished(
        &self,
        window_id: iced::window::Id,
        result: Result<(), String>,
    ) -> Task<Message> {
        if let Err(error) = result {
            push_msg(
                &self.electrs_queue,
                &format!("Tor shutdown completed with a warning: {error}"),
            );
        }
        iced::window::close(window_id)
    }

    fn update_output_viewport_state(
        &mut self,
        pane: OutputPane,
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    ) -> Task<Message> {
        self.output_viewports
            .get_mut(pane)
            .update(offset_y, viewport_height, content_height);
        Task::none()
    }

    fn output(&mut self, message: OutMsg) -> Task<Message> {
        match message {
            OutMsg::PaneHoverChanged { pane, hovered } => {
                if hovered {
                    self.hovered_output_pane = Some(pane);
                } else if self.hovered_output_pane == Some(pane) {
                    self.hovered_output_pane = None;
                }
                Task::none()
            }
            OutMsg::FollowLatest(pane) => {
                self.output_viewports.get_mut(pane).follow_output = true;
                widget::operation::scroll_to(
                    ui_render::output_scroll_id(pane),
                    scrollable::AbsoluteOffset {
                        x: 0.0,
                        y: f32::MAX,
                    },
                )
            }
            OutMsg::PageUp => self.scroll_output_page(-1.0),
            OutMsg::PageDown => self.scroll_output_page(1.0),
        }
    }

    fn path(&mut self, message: PathMsg) -> Task<Message> {
        match message {
            PathMsg::BinariesChanged(path) => {
                if self.paths_are_editable() {
                    self.binaries_path_edit = path;
                }
                Task::none()
            }
            PathMsg::BitcoinDataChanged(path) => {
                if self.paths_are_editable() {
                    self.bitcoin_data_path_edit = path;
                }
                Task::none()
            }
            PathMsg::ElectrsDataChanged(path) => {
                if self.paths_are_editable() {
                    self.electrs_data_path_edit = path;
                }
                Task::none()
            }
            PathMsg::BrowseBinaries => {
                self.browse_path("Select Binaries Folder", Message::BinariesBrowsed)
            }
            PathMsg::BrowseBitcoinData => {
                self.browse_path("Select Bitcoin Data Directory", Message::BitcoinDataBrowsed)
            }
            PathMsg::BrowseElectrsData => {
                self.browse_path("Select Electrs DB Directory", Message::ElectrsDataBrowsed)
            }
            PathMsg::BinariesBrowsed(path) => {
                if let Some(path) = path.filter(|_| self.paths_are_editable()) {
                    self.binaries_path_edit = path;
                }
                Task::none()
            }
            PathMsg::BitcoinDataBrowsed(path) => {
                if let Some(path) = path.filter(|_| self.paths_are_editable()) {
                    self.bitcoin_data_path_edit = path;
                }
                Task::none()
            }
            PathMsg::ElectrsDataBrowsed(path) => {
                if let Some(path) = path.filter(|_| self.paths_are_editable()) {
                    self.electrs_data_path_edit = path;
                }
                Task::none()
            }
            PathMsg::TogglePanel => {
                self.paths_visible = !self.paths_visible;
                Task::none()
            }
        }
    }

    fn browse_path(
        &self,
        title: &'static str,
        map: fn(Option<String>) -> Message,
    ) -> Task<Message> {
        if self.paths_are_editable() {
            Task::perform(async move { browse_folder(title).await }, map)
        } else {
            Task::none()
        }
    }

    fn handle_binary_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenDashboard => self.navigation(NavMsg::OpenDashboard),
            Message::OpenBinaries => self.navigation(NavMsg::OpenBinaries),
            Message::RefreshBinaryInfo => self.navigation(NavMsg::RefreshBinaryInfo),
            Message::InstalledVersionsLoaded {
                request_id,
                versions,
            } => self.apply_installed_versions(request_id, versions),
            Message::AvailableVersionsLoaded {
                request_id,
                versions,
            } => self.apply_available_versions_loaded(request_id, versions),
            Message::SelectBitcoinVersion(version) => self.select_bitcoin_version(version),
            Message::SelectElectrsVersion(version) => self.select_electrs_version(version),
            Message::StartBuild(kind) => self.start_build(kind),
            Message::BuildFinished {
                operation_id,
                result,
            } => self.apply_build_finished(operation_id, result),
            Message::CancelBuild => self.build_update(BuildMsg::Cancel),
            Message::ToggleBuildDetails => self.build_update(BuildMsg::ToggleDetails),
            Message::ToggleBuildAdvanced => self.build_update(BuildMsg::ToggleAdvanced),
            Message::BuildPerformanceChanged(performance) => {
                self.update_build_settings(|settings| settings.performance = performance)
            }
            Message::KeepSourceChanged(keep_source) => {
                self.update_build_settings(|settings| settings.keep_source = keep_source)
            }
            Message::CleanBuildChanged(clean_build) => {
                self.update_build_settings(|settings| settings.clean_build = clean_build)
            }
            Message::VerboseBuildOutputChanged(verbose_output) => {
                self.update_build_settings(|settings| settings.verbose_output = verbose_output)
            }
            Message::RestoreBuildDefaults => {
                self.update_build_settings(|settings| *settings = BuildSettings::default())
            }
            Message::CheckDependencies => self.dependency(DepMsg::Check),
            Message::DependenciesScanned { request_id, report } => {
                self.apply_dependencies_scanned(request_id, report)
            }
            Message::InstallDependencies => self.dependency(DepMsg::Install),
            Message::DependenciesInstalled {
                request_id,
                outcome,
            } => self.apply_dependencies_installed(request_id, outcome),
            Message::ToggleDependencyDetails => self.dependency(DepMsg::ToggleDetails),
            _ => Task::none(),
        }
    }

    fn navigation(&mut self, message: NavMsg) -> Task<Message> {
        match message {
            NavMsg::OpenDashboard => {
                self.page = Page::Dashboard;
                self.hovered_output_pane = None;
                Task::none()
            }
            NavMsg::OpenBinaries => {
                self.page = Page::Binaries;
                self.hovered_output_pane = None;
                self.refresh_binaries_page()
            }
            NavMsg::RefreshBinaryInfo => self.refresh_binaries_page(),
        }
    }

    fn apply_installed_versions(
        &mut self,
        request_id: u64,
        versions: InstalledVersions,
    ) -> Task<Message> {
        if self.binary_page.inventory_request == Some(request_id) {
            self.binary_page.installed_load = InventoryLoad::Idle;
            self.binary_page.installed_versions = Some(versions);
        }
        Task::none()
    }

    fn apply_available_versions_loaded(
        &mut self,
        request_id: u64,
        versions: AvailableVersions,
    ) -> Task<Message> {
        if self.binary_page.inventory_request == Some(request_id) {
            self.apply_available_versions(versions);
        }
        Task::none()
    }

    fn select_bitcoin_version(&mut self, version: ReleaseVersion) -> Task<Message> {
        self.binary_page.selected_bitcoin = Some(version);
        Task::none()
    }

    fn select_electrs_version(&mut self, version: ReleaseVersion) -> Task<Message> {
        self.binary_page.selected_electrs = Some(version);
        Task::none()
    }

    fn build_update(&mut self, message: BuildMsg) -> Task<Message> {
        match message {
            BuildMsg::Cancel => {
                if self.binary_page.can_cancel() && self.build_service.cancel_current() {
                    self.binary_page.cancellation_requested = true;
                }
                Task::none()
            }
            BuildMsg::ToggleDetails => {
                self.binary_page.disclosures.build_details =
                    !self.binary_page.disclosures.build_details;
                if !self.binary_page.disclosures.build_details
                    && self.hovered_output_pane == Some(OutputPane::Build)
                {
                    self.hovered_output_pane = None;
                }
                Task::none()
            }
            BuildMsg::ToggleAdvanced => {
                self.binary_page.disclosures.advanced = !self.binary_page.disclosures.advanced;
                Task::none()
            }
        }
    }

    fn dependency(&mut self, message: DepMsg) -> Task<Message> {
        match message {
            DepMsg::Check => self.check_dependencies(),
            DepMsg::Install => self.install_dependencies(),
            DepMsg::ToggleDetails => {
                self.binary_page.disclosures.dependency_details =
                    !self.binary_page.disclosures.dependency_details;
                Task::none()
            }
        }
    }

    fn apply_dependencies_scanned(
        &mut self,
        request_id: u64,
        report: DependencyReport,
    ) -> Task<Message> {
        if self.binary_page.dependency_request == Some(request_id) {
            self.binary_page.dependency_load = DependencyLoad::Idle;
            self.binary_page.dependency_report = Some(report);
            self.binary_page.dependency_request = None;
            self.binary_page.dependency_message = None;
        }
        Task::none()
    }

    fn apply_dependencies_installed(
        &mut self,
        request_id: u64,
        outcome: DependencyInstallOutcome,
    ) -> Task<Message> {
        if self.binary_page.dependency_request == Some(request_id) {
            self.binary_page.dependency_load = DependencyLoad::Idle;
            self.binary_page.dependency_report = Some(outcome.report);
            self.binary_page.dependency_request = None;
            self.binary_page.dependency_message = Some(outcome.message);
        }
        Task::none()
    }

    fn handle_output_tick(&mut self) -> Task<Message> {
        const MAX: usize = 5_000;
        if self
            .copied_endpoint_at
            .is_some_and(|copied_at| copied_at.elapsed() >= Duration::from_secs(2))
        {
            self.copied_endpoint_at = None;
        }
        self.poll_tor_status_subscription();
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
        if self.should_follow_new_output(OutputPane::Bitcoin, btc_new) {
            tasks.push(widget::operation::scroll_to(
                ui_render::bitcoin_scroll_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: f32::MAX,
                },
            ));
        }
        if self.should_follow_new_output(OutputPane::Electrs, els_new) {
            tasks.push(widget::operation::scroll_to(
                ui_render::electrs_scroll_id(),
                scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: f32::MAX,
                },
            ));
        }
        if self.should_follow_new_output(OutputPane::Build, build_new)
            && self.binary_page.disclosures.build_details
        {
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

    fn poll_tor_status_subscription(&mut self) {
        let update =
            self.tor_status_subscription
                .as_mut()
                .and_then(|status| match status.has_changed() {
                    Ok(true) => Some(Ok(status.borrow_and_update().clone())),
                    Ok(false) => None,
                    Err(error) => Some(Err(error.to_string())),
                });
        match update {
            Some(Ok(status)) => {
                if tor_status_requires_ready_resubmit(&status, &self.connection_readiness()) {
                    self.tor_forwarded_electrs_state = None;
                    self.tor_failed_electrs_state = None;
                    self.tor_electrs_sync_error = None;
                }
                self.tor_status = status;
            }
            Some(Err(error)) => {
                self.tor_status_subscription = None;
                self.tor_runtime_command_in_flight = None;
                let diagnostic = format!("The embedded Tor supervisor stopped: {error}");
                self.tor_status = TorStatus::Error {
                    message: diagnostic.clone(),
                    retryable: false,
                };
                self.tor_forwarded_electrs_state = None;
                self.tor_failed_electrs_state = None;
                self.tor_latest_sync_request = None;
                self.tor_electrs_sync_error = Some(diagnostic.clone());
                self.tor_control_error = Some(diagnostic);
            }
            None => {}
        }
    }

    const fn should_follow_new_output(&self, pane: OutputPane, has_new_output: bool) -> bool {
        has_new_output && self.output_viewports.get(pane).follow_output
    }

    fn scroll_output_page(&mut self, direction: f32) -> Task<Message> {
        let Some(pane) = self.hovered_output_pane else {
            return Task::none();
        };
        let viewport = self.output_viewports.get_mut(pane);
        if !viewport.can_scroll() {
            return Task::none();
        }
        if direction.is_sign_negative() {
            viewport.follow_output = false;
        }

        widget::operation::scroll_by(
            ui_render::output_scroll_id(pane),
            scrollable::AbsoluteOffset {
                x: 0.0,
                y: direction * viewport.page_scroll_distance(),
            },
        )
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
        let managed_electrs_listener = self.active_electrs_listener;
        let listener_already_invalidated = self.electrs_listener_invalidation.is_some();

        Task::perform(
            async move {
                let listener_plan = plan_listener_for_status_poll(
                    electrs_running,
                    managed_electrs_listener,
                    listener_already_invalidated,
                )
                .await;
                if let ElectrsListenerValidation::Invalid(diagnostic) = &listener_plan.validation {
                    return StatusPollResult {
                        identity,
                        bitcoin_probe: None,
                        electrs_status: ElectrsStatus {
                            running: electrs_running,
                            connect_error: Some(diagnostic.clone()),
                            ..ElectrsStatus::default()
                        },
                        electrs_listener_validation: listener_plan.validation,
                    };
                }
                let (bitcoin_probe, electrs_status) = tokio::join!(
                    probe_managed_bitcoin(endpoints.as_ref(), bitcoin_running),
                    electrs_status::probe(
                        electrs_running,
                        managed_bitcoin_rpc,
                        listener_plan.listener_for_probe,
                    ),
                );

                StatusPollResult {
                    identity,
                    bitcoin_probe: Some(bitcoin_probe),
                    electrs_status,
                    electrs_listener_validation: listener_plan.validation,
                }
            },
            |result| Message::StatusPollReceived(Box::new(result)),
        )
    }

    fn apply_status_poll(&mut self, result: StatusPollResult) -> Task<Message> {
        self.reconcile_node_lifecycle();
        if self.active_status_poll != Some(result.identity)
            || self.lifecycle_generation != result.identity.lifecycle_generation
        {
            return Task::none();
        }
        self.active_status_poll = None;

        if self.apply_electrs_listener_validation(result.electrs_listener_validation) {
            return Task::none();
        }

        let retry_rpc = if self.bitcoin_handle.is_some() && self.bitcoin_shutdown.is_none() {
            result
                .bitcoin_probe
                .is_some_and(|probe| self.apply_bitcoin_probe(probe))
        } else {
            self.reset_bitcoin_service_status();
            false
        };

        if self.electrs_handle.is_some() && self.electrs_shutdown.is_none() {
            let mut status = result.electrs_status;
            // Managed running state comes from the owned child handle, never
            // from a network probe that may observe another local service.
            status.running = true;
            if !self.bitcoin_ready() {
                status.connected = false;
                status.synced = false;
                status.ready = false;
                status.bitcoin_error = Some(self.electrs_blocked_error());
                status.connect_error = None;
            }
            if let Some(diagnostic) = self.electrs_listener_invalidation.as_ref() {
                status.connected = false;
                status.synced = false;
                status.ready = false;
                status.connect_error = Some(diagnostic.clone());
            }
            self.apply_electrs_status(status);
        } else {
            self.electrs_status = ElectrsStatus::default();
        }

        if retry_rpc && self.bitcoin_handle.is_some() && self.bitcoin_shutdown.is_none() {
            Task::perform(
                async {
                    tokio::time::sleep(BITCOIN_RPC_STARTUP_RETRY_INTERVAL).await;
                },
                |()| Message::RpcTick,
            )
        } else {
            Task::none()
        }
    }

    fn apply_electrs_listener_validation(&mut self, validation: ElectrsListenerValidation) -> bool {
        let ElectrsListenerValidation::Invalid(diagnostic) = validation else {
            return false;
        };
        if self.electrs_listener_invalidation.is_none() {
            push_msg(
                &self.electrs_queue,
                &format!("Electrs listener invalidated: {diagnostic}"),
            );
            self.electrs_listener_invalidation = Some(diagnostic);
        }
        if self.electrs_handle.is_some() && self.electrs_shutdown.is_none() {
            push_msg(
                &self.electrs_queue,
                "BitEngine is stopping electrs to prevent stale local-network access.",
            );
            let invalidation = self.electrs_listener_invalidation.take();
            self.terminate_electrs_internal();
            self.electrs_listener_invalidation = invalidation;
        }
        true
    }

    fn apply_bitcoin_probe(&mut self, probe: BitcoinProbeResult) -> bool {
        let previous_rpc_error = self.bitcoin_rpc_error.clone();
        let previous_p2p_error = self.bitcoin_p2p_error.clone();
        let previous_compatibility_error = self.bitcoin_compatibility_error.clone();

        let retry_rpc = self.apply_managed_rpc_readiness(&probe.rpc_readiness);
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
            self.bitcoin_blockchain_info = Some(info);
        } else {
            self.bitcoin_synced = false;
            self.block_height = 0;
            self.bitcoin_compatibility_error = None;
            self.bitcoin_blockchain_info = None;
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
        retry_rpc
    }

    fn apply_managed_rpc_readiness(&mut self, readiness: &ManagedRpcReadiness) -> bool {
        match readiness {
            ManagedRpcReadiness::Ready => {
                let had_startup = self.bitcoin_rpc_startup.take().is_some();
                let had_startup_status = self.bitcoin_rpc_startup_status.take().is_some();
                self.bitcoin_rpc_reachable = true;
                self.bitcoin_rpc_error = None;
                if had_startup || had_startup_status {
                    push_msg(&self.bitcoin_queue, "Bitcoin Core RPC is ready.");
                }
                false
            }
            ManagedRpcReadiness::Warmup { status, diagnostic } => {
                self.bitcoin_rpc_reachable = false;
                if self.bitcoin_rpc_startup.is_none() && self.bitcoin_rpc_error.is_some() {
                    false
                } else {
                    self.apply_rpc_startup_wait(status, diagnostic)
                }
            }
            ManagedRpcReadiness::Unavailable { diagnostic } => {
                self.bitcoin_rpc_reachable = false;
                if self.bitcoin_rpc_startup.is_some() {
                    self.apply_rpc_startup_wait("Waiting for managed RPC service…", diagnostic)
                } else {
                    self.bitcoin_rpc_startup_status = None;
                    self.bitcoin_rpc_error = Some(diagnostic.clone());
                    false
                }
            }
            ManagedRpcReadiness::Fatal { diagnostic } => {
                self.bitcoin_rpc_reachable = false;
                self.bitcoin_rpc_startup = None;
                self.bitcoin_rpc_startup_status = None;
                self.bitcoin_rpc_error = Some(diagnostic.clone());
                false
            }
        }
    }

    fn apply_rpc_startup_wait(&mut self, status: &str, diagnostic: &str) -> bool {
        if self.bitcoin_rpc_startup.is_none() {
            self.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());
        }

        let timed_out = self
            .bitcoin_rpc_startup
            .as_ref()
            .is_some_and(|startup| Instant::now() >= startup.deadline);
        if timed_out {
            let Some(startup) = self.bitcoin_rpc_startup.take() else {
                return false;
            };
            let last_status = startup.last_status.as_deref().unwrap_or(status);
            let last_diagnostic = startup.last_diagnostic.as_deref().unwrap_or(diagnostic);
            self.bitcoin_rpc_startup_status = None;
            self.bitcoin_rpc_error = Some(format!(
                "timed out after {}s waiting for managed Bitcoin RPC readiness; last status: {last_status}; last probe: {last_diagnostic}",
                BITCOIN_RPC_STARTUP_TIMEOUT.as_secs()
            ));
            return false;
        }

        let display = format!("Bitcoin Core is starting: {status}");
        if self.bitcoin_rpc_startup_status.as_deref() != Some(display.as_str()) {
            push_msg(&self.bitcoin_queue, &display);
        }
        self.bitcoin_rpc_startup_status = Some(display);
        self.bitcoin_rpc_error = None;
        if let Some(startup) = self.bitcoin_rpc_startup.as_mut() {
            startup.last_status = Some(status.to_owned());
            startup.last_diagnostic = Some(diagnostic.to_owned());
        }
        true
    }

    fn reset_bitcoin_service_status(&mut self) {
        self.bitcoin_synced = false;
        self.bitcoin_rpc_reachable = false;
        self.bitcoin_p2p_reachable = false;
        self.bitcoin_rpc_error = None;
        self.bitcoin_rpc_startup = None;
        self.bitcoin_rpc_startup_status = None;
        self.bitcoin_p2p_error = None;
        self.bitcoin_compatibility_error = None;
        self.bitcoin_blockchain_info = None;
        self.block_height = 0;
    }

    const fn bitcoin_ready(&self) -> bool {
        self.bitcoin_handle.is_some()
            && self.bitcoin_shutdown.is_none()
            && self.bitcoin_rpc_reachable
            && self.bitcoin_p2p_reachable
            && self.bitcoin_compatibility_error.is_none()
    }

    fn connection_readiness(&self) -> ConnectionReadiness {
        ConnectionReadiness::evaluate(
            BitcoinReadiness {
                process_running: self.bitcoin_handle.is_some() && self.bitcoin_shutdown.is_none(),
                blockchain_info: self.bitcoin_blockchain_info.as_ref(),
                error: self
                    .bitcoin_process_error
                    .as_deref()
                    .or(self.bitcoin_compatibility_error.as_deref())
                    .or(self.bitcoin_rpc_error.as_deref()),
                p2p_error: self.bitcoin_p2p_error.as_deref(),
            },
            ElectrsReadiness {
                status: &self.electrs_status,
                process_error: self
                    .electrs_listener_invalidation
                    .as_deref()
                    .or(self.electrs_process_error.as_deref()),
            },
        )
    }

    fn local_endpoint_state(&self) -> LocalEndpointState {
        if self.active_electrs_listener.is_some() {
            if let Some(reason) = self.electrs_listener_invalidation.as_ref() {
                return LocalEndpointState::AddressUnavailable {
                    reason: reason.clone(),
                };
            }
        }
        LocalEndpointState::resolve(
            ElectrsBindPolicy::from(self.config.local_network_access),
            self.active_electrs_listener,
        )
    }

    fn selected_connection_endpoint(&self) -> Option<ElectrumEndpoint> {
        if !self.connection_readiness().is_ready() {
            return None;
        }
        match self.selected_connection_mode {
            ConnectionMode::Local => self.local_endpoint_state().endpoint().cloned(),
            ConnectionMode::Tor => self.available_tor_endpoint(),
        }
    }

    fn available_tor_endpoint(&self) -> Option<ElectrumEndpoint> {
        if self.closing
            || !self.config.tor_enabled
            || !self.tor_runtime_requested
            || self.tor_runtime_stop_pending()
            || self.tor_control_error.is_some()
            || self.tor_electrs_sync_error.is_some()
            || self.electrs_listener_invalidation.is_some()
            || !self.tor_status.is_available()
            || !self.tor_ready_state_is_forwarded()
        {
            return None;
        }
        self.tor_status
            .onion_host()
            .and_then(|host| ElectrumEndpoint::onion(host, ELECTRUM_ONION_PORT).ok())
    }

    fn selected_qr_payload(&self) -> Option<String> {
        if !self.connection_readiness().is_ready() {
            return None;
        }
        match self.selected_connection_mode {
            ConnectionMode::Local => {
                let local = self.local_endpoint_state();
                if local.is_lan_reachable() {
                    local.endpoint().map(ElectrumEndpoint::payload)
                } else {
                    None
                }
            }
            ConnectionMode::Tor => self
                .available_tor_endpoint()
                .map(|endpoint| endpoint.payload()),
        }
    }

    fn refresh_connection_qr(&mut self) {
        self.connection_endpoint = self.selected_connection_endpoint();
        self.connection_endpoint_payload = self
            .connection_endpoint
            .as_ref()
            .map(ElectrumEndpoint::payload);
        let Some(payload) = self.selected_qr_payload() else {
            self.connection_qr = None;
            self.connection_qr_error = None;
            return;
        };
        if self
            .connection_qr
            .as_ref()
            .is_some_and(|qr| qr.payload == payload)
        {
            return;
        }
        match ConnectionQr::encode(payload) {
            Ok(qr) => {
                self.connection_qr = Some(qr);
                self.connection_qr_error = None;
            }
            Err(error) => {
                self.connection_qr = None;
                self.connection_qr_error = Some(error.to_string());
            }
        }
    }

    fn schedule_tor_runtime_command(&mut self, enabled: bool) -> Task<Message> {
        if self.tor_runtime_command_in_flight.is_some() {
            return Task::none();
        }
        let Some(manager) = self.tor_manager.clone() else {
            return Task::none();
        };
        self.tor_runtime_command_in_flight = Some(enabled);
        set_tor_enabled_task(manager, enabled)
    }

    fn finish_tor_runtime_command(
        &mut self,
        completed_state: bool,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.tor_runtime_command_in_flight != Some(completed_state) {
            return Task::none();
        }
        self.tor_runtime_command_in_flight = None;
        match result {
            Ok(()) => {
                self.tor_control_error = None;
                if !completed_state && !self.tor_runtime_requested {
                    self.tor_status = TorStatus::Disabled;
                }
            }
            Err(error) => self.tor_control_error = Some(error),
        }

        if self.tor_runtime_requested == completed_state {
            Task::none()
        } else {
            self.schedule_tor_runtime_command(self.tor_runtime_requested)
        }
    }

    fn tor_runtime_stop_pending(&self) -> bool {
        self.tor_runtime_command_in_flight.is_some()
            && (!self.tor_runtime_requested || self.tor_runtime_command_in_flight == Some(false))
    }

    fn sync_tor_manager(&mut self) -> Vec<Task<Message>> {
        let Some(manager) = self.tor_manager.clone() else {
            return Vec::new();
        };

        let target = match self.tor_electrs_target() {
            Ok(target) => target,
            Err(error) => {
                self.tor_control_error = Some(error);
                return Vec::new();
            }
        };
        let ready = tor_proxy_ready_requested(
            self.tor_runtime_requested,
            self.tor_runtime_stop_pending(),
            self.active_electrs_listener.is_some(),
            &self.connection_readiness(),
            &self.tor_status,
        );
        let state = (target, ready);
        if self
            .tor_latest_sync_request
            .is_some_and(|(_, pending)| pending == state)
        {
            return Vec::new();
        }
        let has_conflicting_request = self.tor_latest_sync_request.is_some();
        if !has_conflicting_request && self.tor_failed_electrs_state == Some(state) {
            return Vec::new();
        }
        if has_conflicting_request || self.tor_forwarded_electrs_state != Some(state) {
            let request_id = self.next_tor_sync_request;
            self.next_tor_sync_request = self.next_tor_sync_request.wrapping_add(1).max(1);
            self.tor_latest_sync_request = Some((request_id, state));
            return vec![set_tor_electrs_state_task(manager, request_id, state)];
        }
        Vec::new()
    }

    fn tor_ready_state_is_forwarded(&self) -> bool {
        self.tor_electrs_target().is_ok_and(|target| {
            self.tor_forwarded_electrs_state == Some((target, true))
                && self.tor_latest_sync_request.is_none()
        })
    }

    fn tor_electrs_target(&self) -> Result<ElectrsTorTarget, String> {
        let socket = self.active_electrs_listener.map_or_else(
            || SocketAddr::from((Ipv4Addr::LOCALHOST, ELECTRUM_ONION_PORT)),
            ElectrsListenAddr::socket_addr,
        );
        ElectrsTorTarget::new(socket).map_err(|error| error.to_string())
    }

    fn bitcoin_dependency_error(&self) -> Option<&str> {
        self.bitcoin_compatibility_error
            .as_deref()
            .or(self.bitcoin_rpc_error.as_deref())
            .or(self.bitcoin_rpc_startup_status.as_deref())
            .or(self.bitcoin_p2p_error.as_deref())
    }

    fn electrs_blocked_error(&self) -> String {
        let (readiness, error) = self
            .bitcoin_compatibility_error
            .as_deref()
            .map(|error| ("compatibility readiness", error))
            .or_else(|| {
                self.bitcoin_rpc_error
                    .as_deref()
                    .map(|error| ("RPC readiness", error))
            })
            .or_else(|| {
                self.bitcoin_p2p_error
                    .as_deref()
                    .map(|error| ("P2P readiness", error))
            })
            .unwrap_or((
                "RPC/P2P readiness",
                "readiness has not yet been confirmed for this managed generation",
            ));
        format!("Electrs is blocked by Bitcoin {readiness}: {error}")
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
            self.bitcoin_process_error = join_result
                .is_err()
                .then(|| "Bitcoin shutdown worker stopped unexpectedly.".to_owned());
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
            self.active_electrs_listener = None;
            self.lan_address = None;
            self.electrs_process_error = join_result
                .is_err()
                .then(|| "Electrs shutdown worker stopped unexpectedly.".to_owned());
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
            let exit_diagnostic = self
                .bitcoin_handle
                .as_mut()
                .and_then(ProcessHandle::take_exit_diagnostic)
                .unwrap_or_else(|| "Bitcoin Core exited unexpectedly.".to_owned());
            let startup_failure = self.bitcoin_rpc_startup.as_ref().map(|startup| {
                startup.last_status.as_deref().map_or_else(
                    || "before managed RPC readiness was confirmed".to_owned(),
                    |status| format!("while RPC was starting: {status}"),
                )
            });
            self.bitcoin_handle = None;
            self.bitcoin_running = false;
            self.reset_bitcoin_service_status();
            self.bitcoin_process_error = Some(exit_diagnostic);
            self.invalidate_electrs_dependency(
                "Bitcoin Core exited; stop Electrs before starting a new Bitcoin generation.",
            );
            if let Some(failure) = startup_failure {
                push_msg(
                    &self.bitcoin_queue,
                    &format!("Bitcoin RPC startup failed: Bitcoin Core exited {failure}."),
                );
            }
            push_msg(&self.bitcoin_queue, "bitcoind has stopped.");
            self.advance_lifecycle_generation();
        }

        let electrs_exited = self
            .electrs_handle
            .as_mut()
            .is_some_and(|handle| !handle.is_running());
        if electrs_exited {
            let exit_diagnostic = self
                .electrs_handle
                .as_mut()
                .and_then(ProcessHandle::take_exit_diagnostic)
                .unwrap_or_else(|| "electrs exited unexpectedly.".to_owned());
            self.electrs_handle = None;
            self.electrs_status = ElectrsStatus::default();
            self.electrs_process_error = Some(exit_diagnostic);
            self.active_electrs_listener = None;
            self.lan_address = None;
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

    const fn config_updates_are_editable(&self) -> bool {
        self.pending_path_save.is_none()
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

    fn installation_recovery_ready(&self) -> bool {
        self.installation_recovery
            .is_ready_for(&self.config.binaries_path)
    }

    fn installation_recovery_block_message(&self) -> String {
        match &self.installation_recovery {
            InstallationRecoveryState::Checking { destination, .. }
                if destination == &self.config.binaries_path =>
            {
                format!(
                    "BitEngine is checking the binary installation at {} for an interrupted transaction. Node launch, inventory, and updates remain locked until the check finishes.",
                    destination.display()
                )
            }
            InstallationRecoveryState::Failed {
                destination,
                error,
            } if destination == &self.config.binaries_path => {
                format!(
                    "Binary installation recovery failed at {}. Node launch, inventory, and updates remain blocked:\n{error}",
                    destination.display()
                )
            }
            InstallationRecoveryState::Checking { .. }
            | InstallationRecoveryState::Ready { .. }
            | InstallationRecoveryState::Failed { .. } => {
                "The configured binaries destination has not completed installation recovery. Node launch, inventory, and updates remain blocked."
                    .to_owned()
            }
        }
    }

    fn begin_installation_recovery(&mut self) -> Task<Message> {
        let request_id = self.next_installation_recovery;
        self.next_installation_recovery = self.next_installation_recovery.wrapping_add(1).max(1);
        let destination = self.config.binaries_path.clone();
        self.installation_recovery = InstallationRecoveryState::Checking {
            request_id,
            destination,
        };
        installation_recovery_task(request_id, self.config.clone())
    }

    fn retry_installation_recovery(&mut self) -> Task<Message> {
        match &self.installation_recovery {
            InstallationRecoveryState::Failed { destination, .. }
                if destination == &self.config.binaries_path =>
            {
                self.overlay_message = None;
                self.begin_installation_recovery()
            }
            InstallationRecoveryState::Checking { .. }
            | InstallationRecoveryState::Ready { .. }
            | InstallationRecoveryState::Failed { .. } => Task::none(),
        }
    }

    fn apply_installation_recovery_finished(
        &mut self,
        request_id: u64,
        destination: PathBuf,
        result: Result<(), String>,
    ) -> Task<Message> {
        if self.closing {
            return Task::none();
        }
        let is_current = matches!(
            &self.installation_recovery,
            InstallationRecoveryState::Checking {
                request_id: current_request,
                destination: current_destination,
            } if *current_request == request_id && current_destination == &destination
        ) && self.config.binaries_path == destination;
        if !is_current {
            return Task::none();
        }

        match result {
            Ok(()) => {
                self.installation_recovery = InstallationRecoveryState::Ready {
                    destination: destination.clone(),
                };
                push_msg(
                    &self.bitcoin_queue,
                    &format!(
                        "Binary installation recovery complete: {}",
                        destination.display()
                    ),
                );
                push_msg(
                    &self.electrs_queue,
                    &format!(
                        "Binary installation recovery complete: {}",
                        destination.display()
                    ),
                );
                if self.page == Page::Binaries {
                    self.refresh_binary_info()
                } else {
                    Task::none()
                }
            }
            Err(error) => {
                let message = format!(
                    "BitEngine could not safely recover the binary installation at {}. Node launch, inventory, and updates remain blocked:\n\n{error}",
                    destination.display()
                );
                self.installation_recovery =
                    InstallationRecoveryState::Failed { destination, error };
                self.overlay_message = Some(message);
                Task::none()
            }
        }
    }

    fn refresh_binaries_page(&mut self) -> Task<Message> {
        Task::batch([self.refresh_binary_info(), self.check_dependencies()])
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
            build_settings: self.config.build_settings,
            theme_preference: self.config.theme_preference,
            local_network_access: self.config.local_network_access,
            tor_enabled: self.config.tor_enabled,
            tor_auto_start: self.config.tor_auto_start,
        };
        if let Err(error) = candidate.validate_paths_lexically() {
            self.overlay_message = Some(format!("Invalid paths:\n{error}"));
            return Task::none();
        }

        let request_id = self.next_path_save;
        self.next_path_save = self.next_path_save.wrapping_add(1).max(1);
        self.pending_path_save = Some(request_id);

        Task::perform(
            async move {
                run_on_detached_thread("bitengine-path-save", move || {
                    candidate
                        .validate_paths()
                        .map_err(|error| error.to_string())?;
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
                    candidate.save().map_err(|error| error.to_string())?;
                    Ok(candidate)
                })
                .await?
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
                let configured_paths_changed = self.config.binaries_path != config.binaries_path
                    || self.config.bitcoin_data_path != config.bitcoin_data_path
                    || self.config.electrs_data_path != config.electrs_data_path;
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
                if configured_paths_changed {
                    self.begin_installation_recovery()
                } else if self.page == Page::Binaries {
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
                self.bitcoin_process_error = None;
                self.managed_endpoints = Some(endpoints);
                self.active_rpc_addr = None;
                self.active_p2p_addr = None;
                self.bitcoin_running = true;
                self.reset_bitcoin_service_status();
                self.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());
                self.advance_lifecycle_generation();
                return Task::done(Message::RpcTick);
            }
            Err(e) => {
                let message = format!("Failed to launch Bitcoin: {e}");
                push_msg(&self.bitcoin_queue, &format!("Launch error: {e}"));
                self.bitcoin_process_error = Some(message.clone());
                self.overlay_message = Some(message);
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
        // A user-initiated launch begins a new listener generation. Failures
        // from the prior generation must not be silently recovered in place,
        // but they no longer apply once an intentional relaunch begins.
        self.electrs_listener_invalidation = None;
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
        let cookie_file = endpoints.cookie_file.clone();

        let bind_policy = ElectrsBindPolicy::from(self.config.local_network_access);
        if bind_policy.requires_lan_address() && !matches!(self.lan_address, Some(Ok(_))) {
            self.electrs_launch_pending_for_lan = true;
            self.lan_address = None;
            return discover_lan_address_task();
        }
        let lan_ip = self
            .lan_address
            .as_ref()
            .and_then(|result| result.as_ref().ok().copied());
        let listener = match ElectrsListenAddr::for_policy(
            bind_policy,
            lan_ip,
            DEFAULT_ELECTRUM_PORT,
        ) {
            Ok(listener) => listener,
            Err(error) => {
                self.electrs_process_error = Some(error.to_string());
                self.overlay_message = Some(format!(
                    "Electrs listener configuration is unavailable:\n{error}\n\nNo listener was started."
                ));
                return Task::none();
            }
        };

        self.start_electrs_process(listener, rpc_addr, p2p_addr, &cookie_file);
        Task::none()
    }

    fn start_electrs_process(
        &mut self,
        listener: ElectrsListenAddr,
        rpc_addr: SocketAddr,
        p2p_addr: SocketAddr,
        cookie_file: &Path,
    ) {
        self.electrs_listener_invalidation = None;
        match process_manager::launch_electrs_with_listener(
            &self.config.binaries_path,
            &self.config.bitcoin_data_path,
            &self.config.electrs_data_path,
            listener,
            ElectrsBitcoinConnection {
                rpc_addr,
                p2p_addr,
                cookie_file,
            },
            &self.electrs_queue,
        ) {
            Ok(handle) => {
                self.electrs_handle = Some(handle);
                self.electrs_process_error = None;
                self.active_electrs_listener = Some(listener);
                self.electrs_status = ElectrsStatus {
                    running: true,
                    ..ElectrsStatus::default()
                };
                self.advance_lifecycle_generation();
            }
            Err(e) => {
                self.active_electrs_listener = None;
                push_msg(&self.electrs_queue, &format!("Launch error: {e}"));
                let message = format!("Failed to launch Electrs: {e}");
                self.electrs_process_error = Some(message.clone());
                self.overlay_message = Some(message);
            }
        }
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
                            std::time::Instant::now() + std::time::Duration::from_mins(1);
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
        if !self.installation_recovery_ready() {
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
        if !self.installation_recovery_ready() {
            self.binary_page.error = Some(self.installation_recovery_block_message());
            return Task::none();
        }
        if self.pending_path_save.is_some() {
            self.binary_page.error = Some(
                "Wait for the pending path save to finish before starting a build.".to_owned(),
            );
            return Task::none();
        }
        if self.binary_page.dependency_load != DependencyLoad::Idle {
            self.binary_page.error = Some(
                "Wait for the dependency check or installation to finish before starting a build."
                    .to_owned(),
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
        self.output_viewports.build = OutputViewportState::default();
        self.binary_page.disclosures.build_details = self.config.build_settings.verbose_output;
        self.binary_page.cancellation_requested = false;
        self.binary_page.error = None;
        self.binary_page.success = None;
        self.binary_page.last_log_path = None;

        let settings = self.config.build_settings;

        let service = self.build_service.clone();
        let event_tx = self.build_event_tx.clone();
        let request = BuildRequest {
            operation_id,
            kind,
            version,
            binaries_dir,
            workspace,
            cores: build_worker_count(kind, settings.performance),
            keep_source: settings.keep_source,
            clean_build: settings.clean_build,
            verbose_output: settings.verbose_output,
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
        if self.installation_recovery_ready() {
            true
        } else {
            self.overlay_message = Some(self.installation_recovery_block_message());
            false
        }
    }

    fn update_build_settings(&mut self, update: impl FnOnce(&mut BuildSettings)) -> Task<Message> {
        if !self.config_updates_are_editable() {
            self.overlay_message = Some(PENDING_PATH_SAVE_SETTINGS_MESSAGE.to_owned());
            return Task::none();
        }
        if self.binary_page.active_operation.is_some() {
            self.overlay_message =
                Some("Build settings cannot be changed while a binary build is active.".to_owned());
            return Task::none();
        }

        let previous = self.config.build_settings;
        update(&mut self.config.build_settings);
        if let Err(error) = self.persist_config() {
            self.config.build_settings = previous;
            self.overlay_message = Some(format!("Failed to save build settings:\n{error:#}"));
        }
        Task::none()
    }

    fn update_ui_preferences(
        &mut self,
        label: &str,
        update: impl FnOnce(&mut Config),
    ) -> Task<Message> {
        if !self.config_updates_are_editable() {
            self.overlay_message = Some(PENDING_PATH_SAVE_SETTINGS_MESSAGE.to_owned());
            return Task::none();
        }
        let previous = self.config.clone();
        update(&mut self.config);
        if let Err(error) = self.persist_config() {
            self.config = previous;
            self.overlay_message = Some(format!("Failed to save {label}:\n{error:#}"));
        }
        Task::none()
    }

    fn persist_config(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(save) = self.config_save_override {
            return save(&self.config);
        }
        self.config.save_preferences()
    }

    fn check_dependencies(&mut self) -> Task<Message> {
        if self.binary_page.active_operation.is_some()
            || self.binary_page.dependency_load != DependencyLoad::Idle
        {
            return Task::none();
        }
        let request_id = self.next_dependency_request;
        self.next_dependency_request = self.next_dependency_request.wrapping_add(1).max(1);
        self.binary_page.dependency_load = DependencyLoad::Checking;
        self.binary_page.dependency_request = Some(request_id);
        self.binary_page.dependency_message = None;
        Task::perform(binaries::scan_build_dependencies(), move |report| {
            Message::DependenciesScanned { request_id, report }
        })
    }

    fn install_dependencies(&mut self) -> Task<Message> {
        if self.binary_page.active_operation.is_some()
            || self.binary_page.dependency_load != DependencyLoad::Idle
        {
            return Task::none();
        }
        let request_id = self.next_dependency_request;
        self.next_dependency_request = self.next_dependency_request.wrapping_add(1).max(1);
        self.binary_page.dependency_load = DependencyLoad::Installing;
        self.binary_page.dependency_request = Some(request_id);
        self.binary_page.dependency_message = None;
        Task::perform(binaries::install_build_dependencies(), move |outcome| {
            Message::DependenciesInstalled {
                request_id,
                outcome,
            }
        })
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
        // An intentional stop clears a prior listener failure. The automatic
        // safety-stop caller temporarily takes and restores its current reason.
        self.electrs_listener_invalidation = None;
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
                    self.active_electrs_listener = None;
                    self.lan_address = None;
                    self.electrs_process_error =
                        Some(format!("Could not start Electrs shutdown worker: {error}"));
                    push_msg(
                        &self.electrs_queue,
                        &format!("Could not start Electrs shutdown worker: {error}"),
                    );
                    self.advance_lifecycle_generation();
                }
            }
        } else {
            self.electrs_status = ElectrsStatus::default();
            self.active_electrs_listener = None;
            self.lan_address = None;
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
            time::every(OUTPUT_POLL_INTERVAL).map(|_| Message::OutputTick),
            time::every(Duration::from_secs(5)).map(|_| Message::RpcTick),
            iced::system::theme_changes().map(Message::SystemThemeChanged),
            iced::window::close_requests().map(Message::WindowCloseRequested),
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::PageUp),
                    ..
                } => Some(Message::OutputPageUp),
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::PageDown),
                    ..
                } => Some(Message::OutputPageDown),
                keyboard::Event::KeyPressed { .. }
                | keyboard::Event::KeyReleased { .. }
                | keyboard::Event::ModifiersChanged(_) => None,
            }),
        ])
    }

    pub fn view(&self) -> Element<'_, Message> {
        ui_render::view(self)
    }

    pub const fn theme(&self) -> iced::Theme {
        match self.config.theme_preference {
            ThemePreference::Light => iced::Theme::Light,
            ThemePreference::Dark => iced::Theme::Dark,
            ThemePreference::System => match self.system_theme {
                iced::theme::Mode::Dark => iced::Theme::Dark,
                iced::theme::Mode::None | iced::theme::Mode::Light => iced::Theme::Light,
            },
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.tor_status_subscription = None;
        if let Some(manager) = self.tor_manager.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    drop(runtime.spawn(async move {
                        let _ = manager.shutdown().await;
                    }));
                }
                Err(_) => drop(manager),
            }
        }

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

fn discover_lan_address_task() -> Task<Message> {
    Task::perform(connection::discover_lan_address(), |result| {
        Message::LanAddressDiscovered(result.map_err(|error| error.to_string()))
    })
}

async fn plan_listener_for_status_poll(
    process_running: bool,
    listener: Option<ElectrsListenAddr>,
    already_invalidated: bool,
) -> ElectrsListenerPollPlan {
    if !process_running || already_invalidated {
        return ElectrsListenerPollPlan {
            listener_for_probe: None,
            validation: ElectrsListenerValidation::NotRequired,
        };
    }
    let Some(listener) = listener else {
        return ElectrsListenerPollPlan {
            listener_for_probe: None,
            validation: ElectrsListenerValidation::NotRequired,
        };
    };
    if listener.policy() == ElectrsBindPolicy::LoopbackOnly {
        return ElectrsListenerPollPlan {
            listener_for_probe: Some(listener),
            validation: ElectrsListenerValidation::NotRequired,
        };
    }
    match connection::revalidate_active_listener(listener).await {
        Ok(()) => ElectrsListenerPollPlan {
            listener_for_probe: Some(listener),
            validation: ElectrsListenerValidation::Valid,
        },
        Err(error) => ElectrsListenerPollPlan {
            listener_for_probe: None,
            validation: ElectrsListenerValidation::Invalid(format!(
                "The active electrs local-network listener could not be revalidated: {error}. BitEngine is stopping electrs to prevent stale local-network access; restart electrs to select a current private address."
            )),
        },
    }
}

fn set_tor_enabled_task(manager: TorManager, enabled: bool) -> Task<Message> {
    let operation = if enabled {
        TorOperation::Enable
    } else {
        TorOperation::Disable
    };
    Task::perform(
        async move {
            manager
                .set_enabled(enabled)
                .await
                .map_err(|error| error.to_string())
        },
        move |result| Message::TorCommandFinished { operation, result },
    )
}

const fn tor_runtime_reconciliation(status: &TorStatus, requested: bool) -> Option<bool> {
    match (status, requested) {
        (TorStatus::Disabled, true) => Some(true),
        (TorStatus::Disabled, false) | (_, true) => None,
        (_, false) => Some(false),
    }
}

const fn tor_status_requires_ready_resubmit(
    status: &TorStatus,
    readiness: &ConnectionReadiness,
) -> bool {
    matches!(status, TorStatus::WaitingForElectrs { .. }) && readiness.is_ready()
}

const fn tor_phase_accepts_ready(status: &TorStatus) -> bool {
    matches!(
        status,
        TorStatus::WaitingForElectrs { .. } | TorStatus::Available { .. }
    )
}

const fn tor_proxy_ready_requested(
    runtime_requested: bool,
    stop_pending: bool,
    active_listener: bool,
    readiness: &ConnectionReadiness,
    status: &TorStatus,
) -> bool {
    runtime_requested
        && !stop_pending
        && active_listener
        && readiness.is_ready()
        && tor_phase_accepts_ready(status)
}

fn retry_tor_task(manager: TorManager) -> Task<Message> {
    Task::perform(
        async move { manager.retry().await.map_err(|error| error.to_string()) },
        |result| Message::TorCommandFinished {
            operation: TorOperation::Retry,
            result,
        },
    )
}

fn shutdown_tor_task(manager: TorManager, window_id: iced::window::Id) -> Task<Message> {
    Task::perform(
        async move { manager.shutdown().await.map_err(|error| error.to_string()) },
        move |result| Message::TorShutdownFinished { window_id, result },
    )
}

fn set_tor_electrs_state_task(
    manager: TorManager,
    request_id: u64,
    state: (ElectrsTorTarget, bool),
) -> Task<Message> {
    let (target, ready) = state;
    Task::perform(
        async move {
            manager
                .set_electrs_state(target, ready)
                .await
                .map_err(|error| error.to_string())
        },
        move |result| Message::TorElectrsStateFinished {
            request_id,
            state,
            result,
        },
    )
}

fn installation_recovery_task(request_id: u64, config: Config) -> Task<Message> {
    let result_destination = config.binaries_path.clone();
    Task::perform(
        async move {
            run_on_detached_thread("bitengine-install-recovery", move || {
                config.validate_paths().map_err(|error| {
                    format!("configured path safety validation failed: {error:#}")
                })?;
                BuildService::ensure_installation_recovered(&config.binaries_path)
            })
            .await?
        },
        move |result| Message::InstallationRecoveryFinished {
            request_id,
            destination: result_destination,
            result,
        },
    )
}

/// Run a potentially unbounded external-volume syscall outside both the Iced
/// UI thread and Tokio's blocking pool.
///
/// Tokio waits indefinitely for `spawn_blocking` work when its owned runtime is
/// dropped. A removable-volume `open(2)` can itself remain blocked indefinitely,
/// so using that pool would make the window responsive but native Close hang.
/// Detaching this dedicated OS thread lets process exit remain authoritative;
/// destination recovery is transactional and will safely resume next launch if
/// the process exits before the worker reports its result.
async fn run_on_detached_thread<T, F>(name: &'static str, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();
    let worker = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = operation();
            let _ = result_tx.send(result);
        })
        .map_err(|error| format!("could not start {name}: {error}"))?;
    drop(worker);

    result_rx
        .await
        .map_err(|_| format!("{name} exited without reporting a result"))
}

// ── Queue helper ──────────────────────────────────────────────────────────────

fn log_startup(
    config: &Config,
    config_warning: Option<&str>,
    bitcoin_queue: &OutputQueue,
    electrs_queue: &OutputQueue,
) {
    push_msg(bitcoin_queue, "=== BitEngine started ===");
    let platform = Platform::current().label();
    push_msg(bitcoin_queue, &format!("Platform : {platform}"));
    if let Some(warning) = config_warning {
        push_msg(bitcoin_queue, warning);
        push_msg(electrs_queue, warning);
    }
    push_msg(
        bitcoin_queue,
        &format!("Config   : {}", Config::config_file_path().display()),
    );
    push_msg(
        bitcoin_queue,
        &format!("Binaries : {}", config.binaries_path.display()),
    );
    log_binary_resolution(bitcoin_queue, &config.binaries_path, "bitcoind");
    push_msg(
        bitcoin_queue,
        &format!("Data dir : {}", config.bitcoin_data_path.display()),
    );

    push_msg(electrs_queue, "=== BitEngine started ===");
    push_msg(electrs_queue, &format!("Platform : {platform}"));
    push_msg(
        electrs_queue,
        &format!("Binaries : {}", config.binaries_path.display()),
    );
    log_binary_resolution(electrs_queue, &config.binaries_path, "electrs");
    push_msg(
        electrs_queue,
        &format!("DB dir   : {}", config.electrs_data_path.display()),
    );
}

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
    push_msg(queue, &format!("Resolved {binary}: {}", resolved.display()));
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
            rpc_readiness: ManagedRpcReadiness::Fatal {
                diagnostic: error.clone(),
            },
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
        rpc_readiness: rpc_probe.readiness,
        p2p_result: p2p_result.map_err(|error| error.to_string()),
    }
}

struct ManagedRpcProbe {
    blockchain_info: Result<BlockchainInfo, String>,
    network_info: Result<NetworkInfo, String>,
    rpc_addr: Option<SocketAddr>,
    readiness: ManagedRpcReadiness,
}

impl ManagedRpcProbe {
    fn without_results(readiness: ManagedRpcReadiness) -> Self {
        let diagnostic = match &readiness {
            ManagedRpcReadiness::Ready => {
                "managed RPC readiness result was internally inconsistent"
            }
            ManagedRpcReadiness::Warmup { diagnostic, .. }
            | ManagedRpcReadiness::Unavailable { diagnostic }
            | ManagedRpcReadiness::Fatal { diagnostic } => diagnostic,
        }
        .to_owned();
        Self {
            blockchain_info: Err(diagnostic.clone()),
            network_info: Err(diagnostic),
            rpc_addr: None,
            readiness,
        }
    }
}

enum RpcProbeFailure {
    Warmup(String),
    Unavailable,
    Fatal,
}

fn classify_rpc_probe_failure(error: &anyhow::Error) -> RpcProbeFailure {
    rpc::rpc_warmup_message(error).map_or_else(
        || {
            if rpc::is_transient_startup_error(error) {
                RpcProbeFailure::Unavailable
            } else {
                RpcProbeFailure::Fatal
            }
        },
        |message| RpcProbeFailure::Warmup(message.to_owned()),
    )
}

async fn probe_managed_rpc(endpoints: &ManagedBitcoinEndpoints) -> ManagedRpcProbe {
    let mut failures = Vec::with_capacity(endpoints.rpc_candidates.len());
    let mut warmup_messages = Vec::new();
    let mut saw_fatal_failure = false;
    for &endpoint in &endpoints.rpc_candidates {
        let auth = match RpcAuth::from_managed_cookie(&endpoints.cookie_file, endpoint) {
            Ok(auth) => auth,
            Err(error) => {
                let diagnostic = error.to_string();
                let readiness = if rpc::is_transient_startup_error(&error) {
                    ManagedRpcReadiness::Unavailable { diagnostic }
                } else {
                    ManagedRpcReadiness::Fatal { diagnostic }
                };
                return ManagedRpcProbe::without_results(readiness);
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
                readiness: ManagedRpcReadiness::Ready,
            };
        }

        for error in [blockchain_info.as_ref().err(), network_info.as_ref().err()]
            .into_iter()
            .flatten()
        {
            match classify_rpc_probe_failure(error) {
                RpcProbeFailure::Warmup(message) => {
                    if !warmup_messages.contains(&message) {
                        warmup_messages.push(message);
                    }
                }
                RpcProbeFailure::Unavailable => {}
                RpcProbeFailure::Fatal => saw_fatal_failure = true,
            }
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

    let details = if failures.is_empty() {
        "no candidate endpoints were configured".to_owned()
    } else {
        failures.join("; ")
    };
    let readiness = if saw_fatal_failure {
        ManagedRpcReadiness::Fatal {
            diagnostic: format!("no managed Bitcoin RPC endpoint was usable ({details})"),
        }
    } else if warmup_messages.is_empty() {
        ManagedRpcReadiness::Unavailable {
            diagnostic: format!("no managed Bitcoin RPC endpoint was reachable ({details})"),
        }
    } else {
        ManagedRpcReadiness::Warmup {
            status: warmup_messages.join("; "),
            diagnostic: format!("managed Bitcoin RPC is still warming up ({details})"),
        }
    };
    ManagedRpcProbe::without_results(readiness)
}

#[cfg(test)]
fn failed_managed_rpc_readiness(error: &str) -> ManagedRpcReadiness {
    ManagedRpcReadiness::Fatal {
        diagnostic: error.to_owned(),
    }
}

fn build_worker_count(kind: BinaryKind, performance: BuildPerformance) -> usize {
    let available = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    performance.jobs(kind, available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[cfg(unix)]
    use std::{os::unix::fs::PermissionsExt as _, sync::atomic::AtomicUsize};

    fn test_app(root: &Path) -> App {
        let mut app = App::from_config(Config::defaults(root), None, root.join("build-state.json"));
        app.config_save_override = Some(|_| Ok(()));
        app.installation_recovery = InstallationRecoveryState::Ready {
            destination: app.config.binaries_path.clone(),
        };
        app
    }

    fn current_installation_recovery(app: &App) -> anyhow::Result<(u64, PathBuf)> {
        match &app.installation_recovery {
            InstallationRecoveryState::Checking {
                request_id,
                destination,
            } => Ok((*request_id, destination.clone())),
            InstallationRecoveryState::Ready { .. } | InstallationRecoveryState::Failed { .. } => {
                anyhow::bail!("installation recovery is not checking")
            }
        }
    }

    #[test]
    fn output_poll_interval_limits_idle_redraws_without_stalling_logs() {
        assert!(
            (Duration::from_millis(500)..=Duration::from_secs(1)).contains(&OUTPUT_POLL_INTERVAL)
        );
    }

    #[test]
    fn app_construction_defers_installation_recovery_until_initial_task() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let config = Config::defaults(temporary.path());
        let lock_path = config.binaries_path.join(".bitengine-install.lock");

        let app = App::from_config(
            config.clone(),
            None,
            temporary.path().join("build-state.json"),
        );

        assert_eq!(
            app.installation_recovery,
            InstallationRecoveryState::Checking {
                request_id: 1,
                destination: config.binaries_path,
            }
        );
        assert!(!lock_path.exists());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn detached_filesystem_worker_does_not_block_the_async_executor() -> anyhow::Result<()> {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = tokio::spawn(run_on_detached_thread(
            "bitengine-test-detached",
            move || {
                let _ = entered_tx.send(());
                let _released = release_rx.recv();
                41_u8
            },
        ));

        entered_rx.await.context("detached worker did not start")?;
        tokio::time::timeout(Duration::from_millis(250), tokio::task::yield_now())
            .await
            .context("detached filesystem work blocked the async executor")?;
        release_tx.send(())?;
        let result = worker.await?.map_err(anyhow::Error::msg)?;
        assert_eq!(result, 41);
        Ok(())
    }

    #[test]
    fn installation_recovery_results_are_correlated_and_fail_closed() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        let (request_id, destination) = current_installation_recovery(&app)?;

        drop(app.apply_installation_recovery_finished(
            request_id.wrapping_add(1),
            destination.clone(),
            Ok(()),
        ));
        assert!(matches!(
            app.installation_recovery,
            InstallationRecoveryState::Checking { .. }
        ));

        drop(app.apply_installation_recovery_finished(
            request_id,
            destination,
            Err("destination lock could not be opened".to_owned()),
        ));
        assert!(matches!(
            &app.installation_recovery,
            InstallationRecoveryState::Failed { error, .. }
                if error == "destination lock could not be opened"
        ));
        assert!(!app.installation_recovery_ready());

        drop(app.retry_installation_recovery());
        let (retry_id, retry_destination) = current_installation_recovery(&app)?;
        assert_ne!(retry_id, request_id);
        drop(app.retry_installation_recovery());
        assert_eq!(
            current_installation_recovery(&app)?,
            (retry_id, retry_destination.clone())
        );

        app.closing = true;
        drop(app.apply_installation_recovery_finished(retry_id, retry_destination, Ok(())));
        assert!(!app.installation_recovery_ready());
        Ok(())
    }

    #[test]
    fn matching_recovery_success_unlocks_destination_consumers() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        let (request_id, destination) = current_installation_recovery(&app)?;
        app.binary_page.selected_bitcoin = Some(
            "v30.0"
                .parse::<ReleaseVersion>()
                .map_err(anyhow::Error::msg)?,
        );

        drop(app.launch_bitcoin());
        assert!(app.bitcoin_handle.is_none());
        app.overlay_message = None;
        drop(app.refresh_binary_info());
        assert!(app.binary_page.inventory_request.is_none());
        drop(app.start_build(BinaryKind::BitcoinCore));
        assert!(app.binary_page.active_operation.is_none());

        drop(app.apply_installation_recovery_finished(request_id, destination, Ok(())));
        assert!(app.installation_recovery_ready());
        Ok(())
    }

    #[test]
    fn old_destination_success_cannot_unlock_a_changed_configuration() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = App::from_config(
            Config::defaults(temporary.path()),
            None,
            temporary.path().join("build-state.json"),
        );
        let (request_id, destination) = current_installation_recovery(&app)?;
        app.config = Config::defaults(&temporary.path().join("changed"));

        drop(app.apply_installation_recovery_finished(request_id, destination, Ok(())));

        assert!(!app.installation_recovery_ready());
        assert!(matches!(
            app.installation_recovery,
            InstallationRecoveryState::Checking { .. }
        ));
        Ok(())
    }

    fn disabled_test_tor_manager(
        root: &Path,
        target: ElectrsTorTarget,
    ) -> anyhow::Result<TorManager> {
        let config = TorManagerConfig::new(root.join("tor-state"), target)?
            .initially_enabled(false)
            .initially_electrs_ready(false);
        Ok(TorManager::spawn(config)?)
    }

    #[test]
    fn theme_preference_resolves_explicit_and_system_modes() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());

        app.config.theme_preference = ThemePreference::System;
        app.system_theme = iced::theme::Mode::Dark;
        assert_eq!(app.theme(), iced::Theme::Dark);
        app.system_theme = iced::theme::Mode::Light;
        assert_eq!(app.theme(), iced::Theme::Light);

        app.config.theme_preference = ThemePreference::Dark;
        app.system_theme = iced::theme::Mode::Light;
        assert_eq!(app.theme(), iced::Theme::Dark);
        app.config.theme_preference = ThemePreference::Light;
        app.system_theme = iced::theme::Mode::Dark;
        assert_eq!(app.theme(), iced::Theme::Light);
        app.config.theme_preference = ThemePreference::System;
        app.system_theme = iced::theme::Mode::None;
        assert_eq!(app.theme(), iced::Theme::Light);
        Ok(())
    }

    #[test]
    fn tor_runtime_request_reconciles_manager_startup_races() {
        assert_eq!(
            tor_runtime_reconciliation(&TorStatus::Disabled, true),
            Some(true)
        );
        assert_eq!(
            tor_runtime_reconciliation(&TorStatus::Starting, false),
            Some(false)
        );
        assert_eq!(
            tor_runtime_reconciliation(&TorStatus::Disabled, false),
            None
        );
        assert_eq!(tor_runtime_reconciliation(&TorStatus::Starting, true), None);
    }

    #[tokio::test]
    async fn rapid_tor_toggle_serializes_the_latest_runtime_request() -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        let manager = disabled_test_tor_manager(temporary.path(), target)?;
        app.tor_manager = Some(manager.clone());
        app.tor_manager_starting = false;

        drop(app.update(Message::TorEnabledChanged(true)));
        assert!(app.config.tor_enabled);
        assert!(app.tor_runtime_requested);
        assert_eq!(app.tor_runtime_command_in_flight, Some(true));

        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        app.tor_forwarded_electrs_state = Some((target, true));

        drop(app.update(Message::TorEnabledChanged(false)));
        assert!(!app.config.tor_enabled);
        assert!(!app.tor_runtime_requested);
        assert_eq!(app.tor_runtime_command_in_flight, Some(true));
        assert!(app.tor_runtime_stop_pending());
        assert!(app.available_tor_endpoint().is_none());

        drop(app.update(Message::TorEnabledChanged(true)));
        assert!(app.config.tor_enabled);
        assert!(app.tor_runtime_requested);
        assert_eq!(app.tor_runtime_command_in_flight, Some(true));
        assert!(!app.tor_runtime_stop_pending());
        assert!(app.available_tor_endpoint().is_none());

        drop(app.update(Message::TorEnabledChanged(false)));
        assert!(!app.config.tor_enabled);
        assert!(!app.tor_runtime_requested);
        assert_eq!(app.tor_runtime_command_in_flight, Some(true));

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Enable,
            result: Ok(()),
        }));
        assert_eq!(app.tor_runtime_command_in_flight, Some(false));
        assert!(app.tor_runtime_stop_pending());

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Enable,
            result: Err("stale enable completion".to_owned()),
        }));
        assert_eq!(app.tor_runtime_command_in_flight, Some(false));
        assert!(app.tor_control_error.is_none());

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Disable,
            result: Ok(()),
        }));
        assert!(app.tor_runtime_command_in_flight.is_none());
        assert!(!app.tor_runtime_stop_pending());
        assert_eq!(app.tor_status, TorStatus::Disabled);

        app.config.tor_enabled = true;
        app.tor_runtime_requested = true;
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        app.tor_forwarded_electrs_state = Some((target, true));
        app.tor_latest_sync_request = None;
        assert!(app.available_tor_endpoint().is_some());

        drop(app.update(Message::TorEnabledChanged(false)));
        assert_eq!(app.tor_runtime_command_in_flight, Some(false));
        assert!(app.tor_runtime_stop_pending());
        assert!(app.available_tor_endpoint().is_none());

        drop(app.update(Message::TorEnabledChanged(true)));
        assert_eq!(app.tor_runtime_command_in_flight, Some(false));
        assert!(app.tor_runtime_stop_pending());
        assert!(app.available_tor_endpoint().is_none());

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Disable,
            result: Ok(()),
        }));
        assert_eq!(app.tor_runtime_command_in_flight, Some(true));
        assert!(app.available_tor_endpoint().is_none());

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Enable,
            result: Ok(()),
        }));
        assert!(app.tor_runtime_command_in_flight.is_none());

        app.tor_manager = None;
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_tor_disable_stays_fail_closed_and_surfaces_control_error() -> anyhow::Result<()>
    {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        let manager = disabled_test_tor_manager(temporary.path(), target)?;
        app.tor_manager = Some(manager.clone());
        app.tor_manager_starting = false;
        app.config.tor_enabled = false;
        app.tor_runtime_requested = false;
        app.tor_runtime_command_in_flight = Some(false);
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };

        drop(app.update(Message::TorCommandFinished {
            operation: TorOperation::Disable,
            result: Err("embedded Tor teardown did not acknowledge".to_owned()),
        }));

        assert!(app.tor_runtime_command_in_flight.is_none());
        assert!(matches!(app.tor_status, TorStatus::Available { .. }));
        assert_eq!(
            app.tor_control_error.as_deref(),
            Some("embedded Tor teardown did not acknowledge")
        );
        assert!(app.available_tor_endpoint().is_none());
        assert!(app.connection_endpoint_payload.is_none());
        assert!(app.connection_qr.is_none());

        drop(app.update(Message::RetryTor));
        assert!(!app.tor_runtime_requested);
        assert!(app.tor_runtime_command_in_flight.is_none());

        app.tor_manager = None;
        manager.shutdown().await?;
        Ok(())
    }

    #[test]
    fn proxy_readiness_is_revoked_while_tor_stop_is_requested_or_pending() {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";
        let status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        let readiness = ConnectionReadiness::Ready;

        assert!(tor_proxy_ready_requested(
            true, false, true, &readiness, &status
        ));
        assert!(!tor_proxy_ready_requested(
            false, false, true, &readiness, &status
        ));
        assert!(!tor_proxy_ready_requested(
            false, true, true, &readiness, &status
        ));
        assert!(!tor_proxy_ready_requested(
            true, true, true, &readiness, &status
        ));
    }

    #[test]
    fn tor_ready_forwarding_is_gated_by_backend_phase() {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let rejected = [
            TorStatus::Disabled,
            TorStatus::Starting,
            TorStatus::Bootstrapping {
                progress: 20,
                summary: None,
            },
            TorStatus::Publishing {
                onion_host: ONION_HOST.to_owned(),
            },
            TorStatus::TemporarilyUnavailable {
                message: "retrying".to_owned(),
                onion_host: Some(ONION_HOST.to_owned()),
                retry_in: None,
            },
            TorStatus::Error {
                message: "failed".to_owned(),
                retryable: false,
            },
        ];
        for status in rejected {
            assert!(!tor_phase_accepts_ready(&status));
        }
        assert!(tor_phase_accepts_ready(&TorStatus::WaitingForElectrs {
            onion_host: ONION_HOST.to_owned(),
        }));
        assert!(tor_phase_accepts_ready(&TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        }));
        assert!(tor_status_requires_ready_resubmit(
            &TorStatus::WaitingForElectrs {
                onion_host: ONION_HOST.to_owned(),
            },
            &ConnectionReadiness::Ready,
        ));
        assert!(!tor_status_requires_ready_resubmit(
            &TorStatus::WaitingForElectrs {
                onion_host: ONION_HOST.to_owned(),
            },
            &ConnectionReadiness::ElectrsStarting,
        ));
    }

    #[tokio::test]
    async fn readiness_loss_preempts_pending_true_and_ignores_its_stale_completion(
    ) -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        let manager = disabled_test_tor_manager(temporary.path(), target)?;
        app.tor_manager = Some(manager.clone());
        app.tor_manager_starting = false;
        app.tor_latest_sync_request = Some((10, (target, true)));
        app.next_tor_sync_request = 11;

        let tasks = app.sync_tor_manager();
        assert_eq!(tasks.len(), 1);
        assert_eq!(app.tor_latest_sync_request, Some((11, (target, false))));
        drop(tasks);

        drop(app.update(Message::TorElectrsStateFinished {
            request_id: 10,
            state: (target, true),
            result: Ok(()),
        }));
        assert_ne!(app.tor_forwarded_electrs_state, Some((target, true)));
        assert_eq!(app.tor_latest_sync_request, Some((11, (target, false))));

        drop(app.update(Message::TorElectrsStateFinished {
            request_id: 11,
            state: (target, false),
            result: Ok(()),
        }));
        assert_eq!(app.tor_forwarded_electrs_state, Some((target, false)));
        assert!(app.tor_latest_sync_request.is_none());

        app.tor_manager = None;
        manager.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn failed_true_sync_still_schedules_false_and_clears_connection_artifacts(
    ) -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        let manager = disabled_test_tor_manager(temporary.path(), target)?;
        app.config.tor_enabled = true;
        app.tor_manager = Some(manager.clone());
        app.tor_manager_starting = false;
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        app.tor_latest_sync_request = Some((20, (target, true)));
        app.next_tor_sync_request = 21;
        let payload = format!("tcp://{ONION_HOST}:50001");
        app.connection_qr = Some(ConnectionQr::encode(payload)?);

        let task = app.update(Message::TorElectrsStateFinished {
            request_id: 20,
            state: (target, true),
            result: Err("proxy state acknowledgement failed".to_owned()),
        });
        assert_eq!(app.tor_latest_sync_request, Some((21, (target, false))));
        assert_eq!(app.tor_failed_electrs_state, Some((target, true)));
        assert!(app.tor_electrs_sync_error.is_some());
        assert!(app.available_tor_endpoint().is_none());
        assert!(app.connection_qr.is_none());
        drop(task);

        app.tor_manager = None;
        manager.shutdown().await?;
        Ok(())
    }

    #[test]
    fn close_during_bootstrap_hides_connection_state_immediately() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.tor_enabled = true;
        app.tor_status = TorStatus::Bootstrapping {
            progress: 51,
            summary: None,
        };

        drop(app.update(Message::WindowCloseRequested(iced::window::Id::unique())));
        assert!(app.closing);
        assert!(app.available_tor_endpoint().is_none());
        assert!(app.connection_qr.is_none());
        Ok(())
    }

    #[test]
    fn tor_endpoint_is_fail_closed_by_preference_and_window_shutdown() -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.tor_enabled = true;
        app.tor_runtime_requested = true;
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        app.tor_forwarded_electrs_state = Some((target, true));
        assert!(app.available_tor_endpoint().is_some());

        app.config.tor_enabled = false;
        assert!(app.available_tor_endpoint().is_none());
        app.config.tor_enabled = true;

        drop(app.update(Message::WindowCloseRequested(iced::window::Id::unique())));
        assert!(app.closing);
        assert!(app.available_tor_endpoint().is_none());
        assert!(app.connection_qr.is_none());
        Ok(())
    }

    #[test]
    fn lan_listener_invalidation_is_sticky_and_hides_local_and_tor_endpoints() -> anyhow::Result<()>
    {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.local_network_access = true;
        app.config.tor_enabled = true;
        app.tor_runtime_requested = true;
        app.active_electrs_listener = Some(ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            Some("192.168.1.42".parse()?),
            DEFAULT_ELECTRUM_PORT,
        )?);
        app.tor_status = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        app.tor_forwarded_electrs_state = Some((target, true));
        assert!(app.local_endpoint_state().endpoint().is_some());
        assert!(app.available_tor_endpoint().is_some());

        let first = "active LAN listener no longer belongs to this host".to_owned();
        app.apply_electrs_listener_validation(ElectrsListenerValidation::Invalid(first.clone()));
        app.apply_electrs_listener_validation(ElectrsListenerValidation::Valid);
        app.apply_electrs_listener_validation(ElectrsListenerValidation::Invalid(
            "later discovery recovered".to_owned(),
        ));

        assert_eq!(
            app.electrs_listener_invalidation.as_deref(),
            Some(first.as_str())
        );
        assert!(app.local_endpoint_state().endpoint().is_none());
        assert!(app.available_tor_endpoint().is_none());
        assert!(matches!(
            ConnectionReadiness::evaluate(
                BitcoinReadiness {
                    process_running: true,
                    blockchain_info: Some(&blockchain_info(100, 100, false, false)),
                    error: None,
                    p2p_error: None,
                },
                ElectrsReadiness {
                    status: &ready_electrs_status(),
                    process_error: app.electrs_listener_invalidation.as_deref(),
                },
            ),
            ConnectionReadiness::ElectrsUnavailable { .. }
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn current_lan_listener_invalidation_stops_only_electrs_and_retains_reason(
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
        app.bitcoin_blockchain_info = Some(blockchain_info(100, 100, false, false));
        app.bitcoin_synced = true;

        let endpoints = app
            .managed_endpoints
            .clone()
            .context("managed endpoint snapshot")?;
        let rpc_addr = endpoints
            .rpc_candidates
            .first()
            .copied()
            .context("managed RPC endpoint")?;
        let p2p_addr = endpoints
            .p2p_candidates
            .first()
            .copied()
            .context("managed P2P endpoint")?;
        let listener = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            Some("192.168.1.42".parse()?),
            DEFAULT_ELECTRUM_PORT,
        )?;
        app.config.local_network_access = true;
        app.start_electrs_process(listener, rpc_addr, p2p_addr, &endpoints.cookie_file);
        let electrs_process_id = wait_for_pids(&electrs_pid_log, 1)?[0];
        app.electrs_status = ready_electrs_status();
        let payload = "tcp://192.168.1.42:50001";
        app.connection_qr = Some(ConnectionQr::encode(payload.to_owned())?);
        assert!(app.local_endpoint_state().endpoint().is_some());

        let identity = StatusPollIdentity {
            request_id: 41,
            lifecycle_generation: app.lifecycle_generation,
        };
        app.active_status_poll = Some(identity);
        let first = "active LAN listener no longer belongs to this host".to_owned();
        drop(
            app.update(Message::StatusPollReceived(Box::new(StatusPollResult {
                identity,
                bitcoin_probe: Some(healthy_bitcoin_probe(blockchain_info(
                    100, 100, false, false,
                ))),
                electrs_status: ready_electrs_status(),
                electrs_listener_validation: ElectrsListenerValidation::Invalid(first.clone()),
            }))),
        );

        assert!(app.electrs_handle.is_none());
        assert!(app.electrs_shutdown.is_some());
        assert!(app.bitcoin_handle.is_some());
        assert!(process_exists(bitcoin_process_id));
        assert_eq!(app.managed_endpoints.as_ref(), Some(&endpoints));
        assert_eq!(
            app.electrs_listener_invalidation.as_deref(),
            Some(first.as_str())
        );
        assert!(app.local_endpoint_state().endpoint().is_none());
        assert!(app.connection_qr.is_none());

        assert!(
            app.apply_electrs_listener_validation(ElectrsListenerValidation::Invalid(
                "later discovery result".to_owned()
            ))
        );
        assert_eq!(
            app.electrs_listener_invalidation.as_deref(),
            Some(first.as_str())
        );
        assert!(app.electrs_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .filter(|line| line.contains("prevent stale local-network access"))
                .count()
                == 1
        }));

        assert_process_exits(electrs_process_id);
        reconcile_until_electrs_shutdown_finishes(&mut app);
        assert!(app.active_electrs_listener.is_none());
        assert!(app.lan_address.is_none());
        assert_eq!(
            app.electrs_listener_invalidation.as_deref(),
            Some(first.as_str())
        );
        assert!(app.bitcoin_handle.is_some());
        assert!(process_exists(bitcoin_process_id));
        assert_eq!(app.managed_endpoints.as_ref(), Some(&endpoints));

        drop(app);
        assert_process_exits(bitcoin_process_id);
        Ok(())
    }

    #[test]
    fn listener_invalidation_resets_only_for_intentional_stop_or_relaunch() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.electrs_listener_invalidation = Some("stale generation".to_owned());

        // A launch request with no active electrs generation clears the old
        // generation even when another prerequisite prevents the new launch.
        drop(app.launch_electrs());
        assert!(app.electrs_listener_invalidation.is_none());

        app.electrs_listener_invalidation = Some("current generation".to_owned());
        app.terminate_electrs_internal();
        assert!(app.electrs_listener_invalidation.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn invalidated_listener_plan_never_returns_a_protocol_probe_target() -> anyhow::Result<()>
    {
        let listener = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LocalNetwork,
            Some("192.168.1.42".parse()?),
            DEFAULT_ELECTRUM_PORT,
        )?;
        let plan = plan_listener_for_status_poll(true, Some(listener), true).await;
        assert!(plan.listener_for_probe.is_none());
        assert_eq!(plan.validation, ElectrsListenerValidation::NotRequired);

        let loopback = ElectrsListenAddr::for_policy(
            ElectrsBindPolicy::LoopbackOnly,
            None,
            DEFAULT_ELECTRUM_PORT,
        )?;
        let loopback_plan = plan_listener_for_status_poll(true, Some(loopback), false).await;
        assert_eq!(loopback_plan.listener_for_probe, Some(loopback));
        assert_eq!(
            loopback_plan.validation,
            ElectrsListenerValidation::NotRequired
        );
        Ok(())
    }

    #[test]
    fn closed_tor_status_channel_invalidates_available_endpoint() -> anyhow::Result<()> {
        const ONION_HOST: &str = "25njqamcweflpvkl73j4szahhihoc4xt3ktcgjnpaingr5yhkenl5sid.onion";

        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.tor_enabled = true;
        app.tor_runtime_requested = true;
        let available = TorStatus::Available {
            onion_host: ONION_HOST.to_owned(),
        };
        let (sender, receiver) = tokio::sync::watch::channel(available.clone());
        drop(sender);
        app.tor_status = available;
        app.tor_status_subscription = Some(receiver);
        let target = app.tor_electrs_target().map_err(anyhow::Error::msg)?;
        app.tor_forwarded_electrs_state = Some((target, true));
        assert!(app.available_tor_endpoint().is_some());

        drop(app.update(Message::OutputTick));

        assert!(app.tor_status_subscription.is_none());
        assert!(matches!(app.tor_status, TorStatus::Error { .. }));
        assert!(app.available_tor_endpoint().is_none());
        assert!(app.connection_qr.is_none());
        Ok(())
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
            rpc_readiness: ManagedRpcReadiness::Ready,
            p2p_result: Ok(SocketAddr::from(([127, 0, 0, 1], 8333))),
        }
    }

    fn failed_bitcoin_probe(error: &str) -> BitcoinProbeResult {
        BitcoinProbeResult {
            blockchain_info: Err(error.to_owned()),
            network_info: Err(error.to_owned()),
            rpc_addr: None,
            rpc_readiness: failed_managed_rpc_readiness(error),
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
        drop(app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe: Some(bitcoin_probe),
            electrs_status,
            electrs_listener_validation: ElectrsListenerValidation::NotRequired,
        }));
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

    fn clear_node_output_queues(app: &App) -> anyhow::Result<()> {
        let Ok(mut bitcoin) = app.bitcoin_queue.lock() else {
            anyhow::bail!("bitcoin output queue was poisoned");
        };
        bitcoin.clear();
        drop(bitcoin);

        let Ok(mut electrs) = app.electrs_queue.lock() else {
            anyhow::bail!("electrs output queue was poisoned");
        };
        electrs.clear();
        Ok(())
    }

    fn update_output_viewport(
        app: &mut App,
        pane: OutputPane,
        offset_y: f32,
        viewport_height: f32,
        content_height: f32,
    ) {
        drop(app.update(Message::OutputViewportChanged {
            pane,
            offset_y,
            viewport_height,
            content_height,
        }));
    }

    #[test]
    fn terminal_at_bottom_follows_new_output() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        clear_node_output_queues(&app)?;
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_000.0);

        // A layout notification can report the newly-grown content before the
        // queued scroll operation runs. It must not look like user scrolling.
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_010.0);
        push_msg(&app.bitcoin_queue, "new output");

        let task = app.handle_output_tick();

        assert_eq!(task.units(), 1);
        assert!(app.output_viewports.bitcoin.follow_output);
        assert_eq!(app.bitcoin_lines, vec!["new output"]);
        Ok(())
    }

    #[test]
    fn terminal_scrolled_up_preserves_viewport_when_output_arrives() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        clear_node_output_queues(&app)?;
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_000.0);
        update_output_viewport(&mut app, OutputPane::Bitcoin, 790.0, 200.0, 1_000.0);
        let preserved_offset = app.output_viewports.bitcoin.offset_y;
        push_msg(&app.bitcoin_queue, "new output while reading history");

        let task = app.handle_output_tick();

        assert_eq!(task.units(), 0);
        assert!(!app.output_viewports.bitcoin.follow_output);
        assert!((app.output_viewports.bitcoin.offset_y - preserved_offset).abs() < f32::EPSILON);
        Ok(())
    }

    #[test]
    fn terminal_bottom_tolerance_ignores_viewport_jitter_for_node_logs() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());

        for pane in [OutputPane::Bitcoin, OutputPane::Electrs] {
            update_output_viewport(&mut app, pane, 800.0, 200.0, 1_000.0);
            update_output_viewport(&mut app, pane, 799.5, 200.0, 1_000.0);

            assert!(app.output_viewports.get(pane).follow_output);
        }

        Ok(())
    }

    #[test]
    fn terminal_scrolled_up_ignores_large_output_burst() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        clear_node_output_queues(&app)?;
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_000.0);
        update_output_viewport(&mut app, OutputPane::Bitcoin, 250.0, 200.0, 1_000.0);

        let Ok(mut queue) = app.bitcoin_queue.lock() else {
            anyhow::bail!("bitcoin output queue was poisoned");
        };
        queue.extend((0..4_000).map(|index| format!("burst line {index}")));
        drop(queue);

        let task = app.handle_output_tick();
        update_output_viewport(&mut app, OutputPane::Bitcoin, 250.0, 200.0, 5_000.0);

        assert_eq!(task.units(), 0);
        assert_eq!(app.bitcoin_lines.len(), 4_000);
        assert!(!app.output_viewports.bitcoin.follow_output);
        assert!((app.output_viewports.bitcoin.offset_y - 250.0).abs() < f32::EPSILON);
        Ok(())
    }

    #[test]
    fn terminal_returned_to_bottom_resumes_following() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        clear_node_output_queues(&app)?;
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_000.0);
        update_output_viewport(&mut app, OutputPane::Bitcoin, 300.0, 200.0, 1_000.0);
        assert!(!app.output_viewports.bitcoin.follow_output);

        update_output_viewport(&mut app, OutputPane::Bitcoin, 798.5, 200.0, 1_000.0);
        assert!(app.output_viewports.bitcoin.follow_output);
        push_msg(&app.bitcoin_queue, "following again");

        assert_eq!(app.handle_output_tick().units(), 1);
        Ok(())
    }

    #[test]
    fn terminal_jump_to_latest_resumes_following() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        update_output_viewport(&mut app, OutputPane::Bitcoin, 800.0, 200.0, 1_000.0);
        update_output_viewport(&mut app, OutputPane::Bitcoin, 300.0, 200.0, 1_000.0);
        assert!(!app.output_viewports.bitcoin.follow_output);

        let task = app.update(Message::OutputFollowLatest(OutputPane::Bitcoin));

        assert_eq!(task.units(), 1);
        assert!(app.output_viewports.bitcoin.follow_output);
        Ok(())
    }

    #[test]
    fn terminal_scroll_inputs_and_page_keys_share_follow_state() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        update_output_viewport(&mut app, OutputPane::Electrs, 800.0, 200.0, 1_000.0);
        drop(app.update(Message::OutputPaneHoverChanged {
            pane: OutputPane::Electrs,
            hovered: true,
        }));

        assert_eq!(app.update(Message::OutputPageUp).units(), 1);
        assert!(!app.output_viewports.electrs.follow_output);
        assert_eq!(app.update(Message::OutputPageDown).units(), 1);

        // Wheel, trackpad, touch, and scrollbar movements all arrive through
        // the scrollable's viewport callback and therefore use this same path.
        update_output_viewport(&mut app, OutputPane::Electrs, 600.0, 200.0, 1_000.0);
        assert!(!app.output_viewports.electrs.follow_output);
        update_output_viewport(&mut app, OutputPane::Electrs, 800.0, 200.0, 1_000.0);
        assert!(app.output_viewports.electrs.follow_output);

        drop(app.update(Message::OutputPaneHoverChanged {
            pane: OutputPane::Electrs,
            hovered: false,
        }));
        assert_eq!(app.update(Message::OutputPageUp).units(), 0);
        Ok(())
    }

    fn alternate_config(root: &Path) -> Config {
        let root = root.join("alternate");
        Config {
            binaries_path: root.join("Binaries"),
            bitcoin_data_path: root.join("BitcoinChain"),
            electrs_data_path: root.join("ElectrsDB"),
            build_settings: BuildSettings::default(),
            theme_preference: crate::config::ThemePreference::default(),
            local_network_access: false,
            tor_enabled: false,
            tor_auto_start: false,
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

    #[derive(Clone, Copy)]
    enum TestRpcReply {
        Success,
        Warmup(&'static str),
        Error { code: i64, message: &'static str },
        Unauthorized,
        Malformed,
    }

    fn test_rpc_response(method: &str, reply: TestRpcReply) -> anyhow::Result<String> {
        let (status, body) = match reply {
            TestRpcReply::Success => {
                let result = match method {
                    "getblockchaininfo" => serde_json::json!({
                        "blocks": 100,
                        "headers": 100,
                        "verificationprogress": 1.0,
                        "initialblockdownload": false,
                        "pruned": false
                    }),
                    "getnetworkinfo" => serde_json::json!({
                        "version": 300_000,
                        "networkactive": true
                    }),
                    unexpected => anyhow::bail!("unexpected test RPC method {unexpected}"),
                };
                (
                    "200 OK",
                    serde_json::json!({"result": result, "error": null}).to_string(),
                )
            }
            TestRpcReply::Warmup(message) => (
                "500 Internal Server Error",
                serde_json::json!({
                    "result": null,
                    "error": {"code": -28, "message": message}
                })
                .to_string(),
            ),
            TestRpcReply::Error { code, message } => (
                "500 Internal Server Error",
                serde_json::json!({
                    "result": null,
                    "error": {"code": code, "message": message}
                })
                .to_string(),
            ),
            TestRpcReply::Unauthorized => ("401 Unauthorized", String::new()),
            TestRpcReply::Malformed => ("200 OK", "{not valid JSON".to_owned()),
        };
        Ok(format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    async fn spawn_scripted_rpc_server(
        bind: &str,
        blockchain_replies: Vec<TestRpcReply>,
        network_replies: Vec<TestRpcReply>,
    ) -> anyhow::Result<(
        SocketAddr,
        tokio::task::JoinHandle<anyhow::Result<Vec<String>>>,
    )> {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let endpoint = listener.local_addr()?;
        let request_count = blockchain_replies.len() + network_replies.len();
        let server = tokio::spawn(async move {
            let mut methods = Vec::with_capacity(request_count);
            let mut blockchain_index = 0;
            let mut network_index = 0;
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await?;
                let request = read_test_rpc_request(&mut stream).await?;
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .context("test RPC request omitted method")?;
                let reply = match method {
                    "getblockchaininfo" => {
                        let reply = blockchain_replies
                            .get(blockchain_index)
                            .copied()
                            .context("unexpected extra getblockchaininfo request")?;
                        blockchain_index += 1;
                        reply
                    }
                    "getnetworkinfo" => {
                        let reply = network_replies
                            .get(network_index)
                            .copied()
                            .context("unexpected extra getnetworkinfo request")?;
                        network_index += 1;
                        reply
                    }
                    unexpected => anyhow::bail!("unexpected test RPC method {unexpected}"),
                };
                stream
                    .write_all(test_rpc_response(method, reply)?.as_bytes())
                    .await?;
                methods.push(method.to_owned());
            }
            Ok(methods)
        });
        Ok((endpoint, server))
    }

    fn managed_rpc_endpoints(
        cookie_file: PathBuf,
        rpc_candidates: Vec<SocketAddr>,
    ) -> ManagedBitcoinEndpoints {
        ManagedBitcoinEndpoints {
            rpc_port: rpc_candidates.first().map_or(8332, SocketAddr::port),
            rpc_candidates,
            p2p_candidates: Vec::new(),
            cookie_file,
        }
    }

    fn bitcoin_probe_from_rpc(probe: ManagedRpcProbe) -> BitcoinProbeResult {
        BitcoinProbeResult {
            blockchain_info: probe.blockchain_info,
            network_info: probe.network_info,
            rpc_addr: probe.rpc_addr,
            rpc_readiness: probe.readiness,
            p2p_result: Ok(SocketAddr::from(([127, 0, 0, 1], 8333))),
        }
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

    #[tokio::test]
    async fn blockchain_warmup_retries_until_managed_rpc_becomes_ready() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![
                TestRpcReply::Warmup("Loading block index…"),
                TestRpcReply::Warmup("Loading block index…"),
                TestRpcReply::Warmup("Loading block index…"),
                TestRpcReply::Success,
            ],
            vec![TestRpcReply::Success; 4],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);
        let mut app = test_app(temporary.path());
        app.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());

        for _ in 0..3 {
            let probe = probe_managed_rpc(&endpoints).await;
            assert!(matches!(
                &probe.readiness,
                ManagedRpcReadiness::Warmup { status, .. }
                    if status == "Loading block index…"
            ));
            assert!(app.apply_bitcoin_probe(bitcoin_probe_from_rpc(probe)));
            assert!(app.bitcoin_rpc_error.is_none());
            assert!(!app.bitcoin_rpc_reachable);
        }

        let ready = probe_managed_rpc(&endpoints).await;
        assert_eq!(ready.readiness, ManagedRpcReadiness::Ready);
        assert!(!app.apply_bitcoin_probe(bitcoin_probe_from_rpc(ready)));
        assert!(app.bitcoin_rpc_reachable);
        assert_eq!(app.active_rpc_addr, Some(endpoint));
        assert!(app.bitcoin_rpc_startup.is_none());
        assert!(app.bitcoin_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .filter(|line| line.as_str() == "Bitcoin Core is starting: Loading block index…")
                .count()
                == 1
                && !lines
                    .iter()
                    .any(|line| line.contains("RPC readiness check failed"))
        }));

        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("scripted RPC server timed out")???;
        assert_eq!(methods.len(), 8);
        Ok(())
    }

    #[tokio::test]
    async fn network_warmup_retries_until_required_call_succeeds() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Success; 2],
            vec![
                TestRpcReply::Warmup("Loading P2P addresses…"),
                TestRpcReply::Success,
            ],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);
        let mut app = test_app(temporary.path());
        app.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());

        let warmup = probe_managed_rpc(&endpoints).await;
        assert!(matches!(
            &warmup.readiness,
            ManagedRpcReadiness::Warmup { status, .. }
                if status == "Loading P2P addresses…"
        ));
        assert!(app.apply_bitcoin_probe(bitcoin_probe_from_rpc(warmup)));

        let ready = probe_managed_rpc(&endpoints).await;
        assert_eq!(ready.readiness, ManagedRpcReadiness::Ready);
        assert!(!app.apply_bitcoin_probe(bitcoin_probe_from_rpc(ready)));
        assert!(app.bitcoin_rpc_reachable);

        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("scripted RPC server timed out")???;
        assert_eq!(methods.len(), 4);
        Ok(())
    }

    #[tokio::test]
    async fn ipv4_and_ipv6_warmup_is_one_informational_startup_state() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let replies = vec![TestRpcReply::Warmup("Loading block index…")];
        let (ipv4, ipv4_server) =
            spawn_scripted_rpc_server("127.0.0.1:0", replies.clone(), replies.clone()).await?;
        let (ipv6, ipv6_server) =
            spawn_scripted_rpc_server("[::1]:0", replies.clone(), replies).await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![ipv4, ipv6]);
        let mut app = test_app(temporary.path());
        app.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());

        let probe = probe_managed_rpc(&endpoints).await;
        assert!(matches!(
            &probe.readiness,
            ManagedRpcReadiness::Warmup { status, .. }
                if status == "Loading block index…"
        ));
        assert!(app.apply_bitcoin_probe(bitcoin_probe_from_rpc(probe)));
        assert!(app.bitcoin_rpc_error.is_none());
        assert!(app.bitcoin_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .filter(|line| line.as_str() == "Bitcoin Core is starting: Loading block index…")
                .count()
                == 1
                && !lines
                    .iter()
                    .any(|line| line.contains("RPC readiness check failed"))
        }));

        for server in [ipv4_server, ipv6_server] {
            let methods = tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .context("dual-stack RPC server timed out")???;
            assert_eq!(methods.len(), 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn warmup_candidate_falls_through_to_ready_managed_endpoint() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (warmup_endpoint, warmup_server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Warmup("Loading block index…")],
            vec![TestRpcReply::Warmup("Loading block index…")],
        )
        .await?;
        let (ready_endpoint, ready_server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Success],
            vec![TestRpcReply::Success],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![warmup_endpoint, ready_endpoint]);

        let probe = probe_managed_rpc(&endpoints).await;

        assert_eq!(probe.readiness, ManagedRpcReadiness::Ready);
        assert_eq!(probe.rpc_addr, Some(ready_endpoint));
        for server in [warmup_server, ready_server] {
            let methods = tokio::time::timeout(Duration::from_secs(2), server)
                .await
                .context("fallback RPC server timed out")???;
            assert_eq!(methods.len(), 2);
        }
        Ok(())
    }

    #[tokio::test]
    async fn persistent_warmup_reaches_clean_startup_timeout() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Warmup("Loading block index…"); 3],
            vec![TestRpcReply::Warmup("Loading block index…"); 3],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);
        let mut app = test_app(temporary.path());
        app.bitcoin_rpc_startup = Some(BitcoinRpcStartup::new());

        for _ in 0..2 {
            let probe = probe_managed_rpc(&endpoints).await;
            assert!(app.apply_bitcoin_probe(bitcoin_probe_from_rpc(probe)));
        }
        app.bitcoin_rpc_startup
            .as_mut()
            .context("active RPC startup state")?
            .deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .context("construct elapsed startup deadline")?;

        let timed_out = probe_managed_rpc(&endpoints).await;
        assert!(!app.apply_bitcoin_probe(bitcoin_probe_from_rpc(timed_out)));
        assert!(!app.bitcoin_rpc_reachable);
        assert!(app.bitcoin_rpc_startup.is_none());
        assert!(app.bitcoin_rpc_error.as_deref().is_some_and(|error| {
            error.contains("timed out after 300s")
                && error.contains("Loading block index…")
                && error.contains(&endpoint.to_string())
                && error.contains("RPC error -28")
        }));
        assert!(app.bitcoin_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .filter(|line| line.as_str() == "Bitcoin Core is starting: Loading block index…")
                .count()
                == 1
                && lines.iter().any(|line| {
                    line.contains("RPC readiness check failed") && line.contains("timed out")
                })
        }));

        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("persistent warmup RPC server timed out")???;
        assert_eq!(methods.len(), 6);
        Ok(())
    }

    #[tokio::test]
    async fn authentication_error_remains_fatal_during_startup() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:wrong-password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Unauthorized],
            vec![TestRpcReply::Unauthorized],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);

        let probe = probe_managed_rpc(&endpoints).await;

        assert!(matches!(
            &probe.readiness,
            ManagedRpcReadiness::Fatal { diagnostic }
                if diagnostic.contains("RPC authentication failed (401)")
        ));
        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("authentication RPC server timed out")???;
        assert_eq!(methods.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn unrelated_rpc_error_remains_fatal_during_startup() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Error {
                code: -1,
                message: "unexpected failure",
            }],
            vec![TestRpcReply::Error {
                code: -1,
                message: "unexpected failure",
            }],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);

        let probe = probe_managed_rpc(&endpoints).await;

        assert!(matches!(
            &probe.readiness,
            ManagedRpcReadiness::Fatal { diagnostic }
                if diagnostic.contains("RPC error -1: unexpected failure")
        ));
        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("fatal-error RPC server timed out")???;
        assert_eq!(methods.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_rpc_response_remains_fatal_during_startup() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Malformed],
            vec![TestRpcReply::Malformed],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);

        let probe = probe_managed_rpc(&endpoints).await;

        assert!(matches!(
            &probe.readiness,
            ManagedRpcReadiness::Fatal { diagnostic }
                if diagnostic.contains("parse RPC response")
        ));
        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("malformed RPC server timed out")???;
        assert_eq!(methods.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn already_ready_managed_rpc_succeeds_without_startup_delay() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let cookie_file = temporary.path().join(".cookie");
        std::fs::write(&cookie_file, "user:password\n")?;
        let (endpoint, server) = spawn_scripted_rpc_server(
            "127.0.0.1:0",
            vec![TestRpcReply::Success],
            vec![TestRpcReply::Success],
        )
        .await?;
        let endpoints = managed_rpc_endpoints(cookie_file, vec![endpoint]);

        let probe = probe_managed_rpc(&endpoints).await;

        assert_eq!(probe.readiness, ManagedRpcReadiness::Ready);
        assert_eq!(probe.rpc_addr, Some(endpoint));
        assert!(probe.blockchain_info.is_ok());
        assert!(probe.network_info.is_ok());
        let methods = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .context("ready RPC server timed out")???;
        assert_eq!(methods.len(), 2);
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

    #[cfg(unix)]
    fn reconcile_until_electrs_shutdown_finishes(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.electrs_shutdown.is_some() && Instant::now() < deadline {
            app.reconcile_node_lifecycle();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.electrs_shutdown.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn process_exit_ends_rpc_warmup_immediately() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let bitcoin_pid_log = temporary.path().join("bitcoin-pids");
        install_node_helper(temporary.path(), "bitcoind", &bitcoin_pid_log)?;
        let mut app = test_app(temporary.path());
        drop(app.launch_bitcoin());
        let process_id = wait_for_pids(&bitcoin_pid_log, 1)?[0];
        let warmup = ManagedRpcProbe::without_results(ManagedRpcReadiness::Warmup {
            status: "Loading block index…".to_owned(),
            diagnostic: "managed RPC returned RPC error -28".to_owned(),
        });
        assert!(app.apply_bitcoin_probe(bitcoin_probe_from_rpc(warmup)));

        // SAFETY: the PID belongs to the helper process launched above.
        assert_eq!(unsafe { libc::kill(process_id, libc::SIGKILL) }, 0);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.bitcoin_handle.is_some() && Instant::now() < deadline {
            app.reconcile_node_lifecycle();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(app.bitcoin_handle.is_none());
        assert!(!app.bitcoin_running);
        assert!(app.bitcoin_queue.lock().is_ok_and(|lines| lines.iter().any(
            |line| line
                == "Bitcoin RPC startup failed: Bitcoin Core exited while RPC was starting: Loading block index…."
        )));
        Ok(())
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
    fn pending_path_save_serializes_config_messages_until_completion() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.theme_preference = ThemePreference::Light;
        app.config.local_network_access = false;
        app.config.tor_enabled = true;
        app.config.tor_auto_start = true;
        app.config.build_settings.performance = BuildPerformance::Low;
        app.tor_runtime_requested = true;
        app.tor_status = TorStatus::Starting;
        app.lan_address = Some(Ok("192.168.50.8".parse()?));

        let mut saved_candidate = alternate_config(temporary.path());
        saved_candidate.theme_preference = app.config.theme_preference;
        saved_candidate.local_network_access = app.config.local_network_access;
        saved_candidate.tor_enabled = app.config.tor_enabled;
        saved_candidate.tor_auto_start = app.config.tor_auto_start;
        saved_candidate.build_settings = app.config.build_settings;
        app.pending_path_save = Some(41);

        drop(app.update(Message::TorEnabledChanged(false)));
        drop(app.update(Message::ThemePreferenceChanged(ThemePreference::Dark)));
        drop(app.update(Message::LocalNetworkAccessChanged(true)));
        drop(app.update(Message::TorAutoStartChanged(false)));
        drop(app.update(Message::BuildPerformanceChanged(BuildPerformance::Fastest)));

        assert_eq!(app.config.theme_preference, ThemePreference::Light);
        assert!(!app.config.local_network_access);
        assert!(app.config.tor_enabled);
        assert!(app.config.tor_auto_start);
        assert_eq!(app.config.build_settings.performance, BuildPerformance::Low);
        assert!(app.tor_runtime_requested);
        assert_eq!(app.tor_status, TorStatus::Starting);
        assert_eq!(app.lan_address, Some(Ok("192.168.50.8".parse()?)));
        assert_eq!(
            app.overlay_message.as_deref(),
            Some(PENDING_PATH_SAVE_SETTINGS_MESSAGE)
        );

        drop(app.apply_paths_saved(41, Ok(saved_candidate)));
        assert!(app.pending_path_save.is_none());

        drop(app.update(Message::TorEnabledChanged(false)));
        drop(app.update(Message::ThemePreferenceChanged(ThemePreference::Dark)));
        drop(app.update(Message::LocalNetworkAccessChanged(true)));
        drop(app.update(Message::TorAutoStartChanged(false)));
        drop(app.update(Message::BuildPerformanceChanged(BuildPerformance::Fastest)));

        assert_eq!(app.config.theme_preference, ThemePreference::Dark);
        assert!(app.config.local_network_access);
        assert!(!app.config.tor_enabled);
        assert!(!app.config.tor_auto_start);
        assert_eq!(
            app.config.build_settings.performance,
            BuildPerformance::Fastest
        );
        assert!(!app.tor_runtime_requested);
        assert_eq!(app.tor_status, TorStatus::Starting);
        assert!(app.lan_address.is_none());
        Ok(())
    }

    #[test]
    fn stale_paths_saved_result_cannot_revert_preferences() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.config.theme_preference = ThemePreference::Dark;
        app.config.local_network_access = true;
        app.config.tor_enabled = false;
        app.config.tor_auto_start = false;
        app.config.build_settings.performance = BuildPerformance::Fastest;
        app.pending_path_save = Some(52);

        let mut stale = alternate_config(temporary.path());
        stale.theme_preference = ThemePreference::System;
        stale.local_network_access = false;
        stale.tor_enabled = true;
        stale.tor_auto_start = true;
        stale.build_settings.performance = BuildPerformance::Low;
        drop(app.apply_paths_saved(51, Ok(stale)));

        assert_eq!(app.pending_path_save, Some(52));
        assert_eq!(app.config.theme_preference, ThemePreference::Dark);
        assert!(app.config.local_network_access);
        assert!(!app.config.tor_enabled);
        assert!(!app.config.tor_auto_start);
        assert_eq!(
            app.config.build_settings.performance,
            BuildPerformance::Fastest
        );
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
        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
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

        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, true, false,
        )));
        assert!(!app.bitcoin_synced, "IBD must remain unsynchronized");

        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            99, 100, false, false,
        )));
        assert!(
            !app.bitcoin_synced,
            "a one-block lag must remain unsynchronized"
        );

        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, false, false,
        )));
        assert!(app.bitcoin_synced);
        assert!(app.bitcoin_compatibility_error.is_none());

        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
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
        let _ = app.apply_bitcoin_probe(inactive);
        assert!(app
            .bitcoin_compatibility_error
            .as_deref()
            .is_some_and(|error| error.contains("setnetworkactive true")));
        Ok(())
    }

    #[test]
    fn transient_p2p_startup_failure_recovers_on_a_later_status_poll() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        let endpoint = SocketAddr::from(([127, 0, 0, 1], 8333));
        let mut startup = healthy_bitcoin_probe(blockchain_info(100, 100, false, false));
        startup.p2p_result = Err(format!(
            "no configured Bitcoin P2P endpoint completed a mainnet handshake: {endpoint} — TCP connection refused"
        ));

        let _ = app.apply_bitcoin_probe(startup);
        assert!(!app.bitcoin_p2p_reachable);
        assert!(app
            .bitcoin_p2p_error
            .as_deref()
            .is_some_and(|error| error.contains("TCP connection refused")));

        let _ = app.apply_bitcoin_probe(healthy_bitcoin_probe(blockchain_info(
            100, 100, false, false,
        )));
        assert!(app.bitcoin_p2p_reachable);
        assert!(app.bitcoin_p2p_error.is_none());
        assert_eq!(app.active_p2p_addr, Some(endpoint));
        assert!(app.bitcoin_queue.lock().is_ok_and(|lines| {
            lines
                .iter()
                .any(|line| line.contains("P2P readiness check recovered"))
        }));
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
        assert!(app
            .electrs_status
            .bitcoin_error
            .as_deref()
            .is_some_and(|error| error.contains("blocked by Bitcoin RPC readiness")));
        assert!(app.electrs_status.connect_error.is_none());
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
                .any(|line| line.contains("Bitcoin check recovered"))
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

        drop(app.apply_status_poll(StatusPollResult {
            identity: stale_identity,
            bitcoin_probe: Some(healthy_bitcoin_probe(blockchain_info(
                100, 100, false, false,
            ))),
            electrs_status: ready_electrs_status(),
            electrs_listener_validation: ElectrsListenerValidation::NotRequired,
        }));
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
        let electrs_process_id = wait_for_pids(&electrs_pid_log, 1)?[0];

        drop(app.apply_status_poll(StatusPollResult {
            identity: stale_identity,
            bitcoin_probe: Some(failed_bitcoin_probe("stale poll")),
            electrs_status: ElectrsStatus::default(),
            electrs_listener_validation: ElectrsListenerValidation::Invalid(
                "stale listener invalidation".to_owned(),
            ),
        }));

        assert!(app.electrs_handle.is_some());
        assert!(app.electrs_shutdown.is_none());
        assert!(app.electrs_status.running);
        assert!(app.electrs_listener_invalidation.is_none());
        assert!(process_exists(electrs_process_id));
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

        drop(app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe: Some(failed_bitcoin_probe("RPC timed out")),
            electrs_status: ElectrsStatus::default(),
            electrs_listener_validation: ElectrsListenerValidation::NotRequired,
        }));

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

        drop(app.apply_status_poll(StatusPollResult {
            identity,
            bitcoin_probe: Some(healthy_bitcoin_probe(blockchain_info(
                100, 100, false, false,
            ))),
            electrs_status: ElectrsStatus {
                running: true,
                connected: true,
                ready: true,
                ..ElectrsStatus::default()
            },
            electrs_listener_validation: ElectrsListenerValidation::NotRequired,
        }));

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
