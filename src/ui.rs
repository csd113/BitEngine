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
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
    time::Duration,
};

#[path = "ui_render.rs"]
mod ui_render;

use iced::{
    time,
    widget::{self, scrollable},
    Element, Subscription, Task,
};

use crate::{
    binaries::{
        self, AvailableVersions, BinaryKind, BuildEvent, BuildFailure, BuildRequest, BuildService,
        BuildStage, BuildSummary, InstalledVersions, PersistedBuild, PersistedBuildStatus,
        ReleaseVersion,
    },
    config::Config,
    electrs_status::{self, ElectrsStatus},
    platform::{self, Platform},
    process_manager::{self, new_queue, OutputQueue, ProcessHandle},
    rpc::{self, BlockchainInfo, RpcAuth},
};

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
    PathsSaved(Result<(), String>),
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
    InstalledVersionsLoaded(InstalledVersions),
    AvailableVersionsLoaded(AvailableVersions),
    SelectBitcoinVersion(ReleaseVersion),
    SelectElectrsVersion(ReleaseVersion),
    StartBuild(BinaryKind),
    BuildFinished(Result<BuildSummary, BuildFailure>),
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
    active_kind: Option<BinaryKind>,
    stage: Option<BuildStage>,
    progress: f32,
    log_lines: Vec<String>,
    disclosures: BinaryDisclosures,
    cancellation_requested: bool,
    error: Option<String>,
    success: Option<String>,
    last_log_path: Option<PathBuf>,
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
            active_kind: None,
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
                    state.error = Some(
                        "The previous build did not finish. Existing binaries were left unchanged."
                            .to_owned(),
                    );
                }
            }
        }
        state
    }
}

// ── App state ─────────────────────────────────────────────────────────────────

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

    // ── Output queues (filled by background threads, drained by OutputTick) ──
    bitcoin_queue: OutputQueue,
    electrs_queue: OutputQueue,

    // ── Terminal display buffers ───────────────────────────────────────────────
    bitcoin_lines: Vec<String>,
    electrs_lines: Vec<String>,

    // ── Node status ───────────────────────────────────────────────────────────
    bitcoin_running: bool,
    bitcoin_synced: bool,
    electrs_status: ElectrsStatus,
    block_height: u64,

    // ── UI state ──────────────────────────────────────────────────────────────
    page: Page,
    paths_visible: bool,
    binary_page: BinaryPageState,
    build_service: BuildService,
    build_event_tx: mpsc::Sender<BuildEvent>,
    build_event_rx: mpsc::Receiver<BuildEvent>,

    /// Non-empty ⇒ display an overlay dialog with this message.
    overlay_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StatusPollResult {
    blockchain_info: Result<BlockchainInfo, String>,
    electrs_status: ElectrsStatus,
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
        let (build_event_tx, build_event_rx) = mpsc::channel();
        let build_service = BuildService::new(build_state_path);
        let binary_page = BinaryPageState::new(build_service.recovered());

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
            bitcoin_queue,
            electrs_queue,
            bitcoin_lines: Vec::new(),
            electrs_lines: Vec::new(),
            bitcoin_running: false,
            bitcoin_synced: false,
            electrs_status: ElectrsStatus::default(),
            block_height: 0,
            page: Page::Dashboard,
            paths_visible: true,
            binary_page,
            build_service,
            build_event_tx,
            build_event_rx,
            overlay_message: None,
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
                self.binaries_path_edit = s;
                Task::none()
            }
            Message::BitcoinDataPathChanged(s) => {
                self.bitcoin_data_path_edit = s;
                Task::none()
            }
            Message::ElectrsDataPathChanged(s) => {
                self.electrs_data_path_edit = s;
                Task::none()
            }
            Message::BrowseBinaries => Task::perform(
                async { browse_folder("Select Binaries Folder").await },
                Message::BinariesBrowsed,
            ),
            Message::BrowseBitcoinData => Task::perform(
                async { browse_folder("Select Bitcoin Data Directory").await },
                Message::BitcoinDataBrowsed,
            ),
            Message::BrowseElectrsData => Task::perform(
                async { browse_folder("Select Electrs DB Directory").await },
                Message::ElectrsDataBrowsed,
            ),
            Message::BinariesBrowsed(p) => {
                if let Some(s) = p {
                    self.binaries_path_edit = s;
                }
                Task::none()
            }
            Message::BitcoinDataBrowsed(p) => {
                if let Some(s) = p {
                    self.bitcoin_data_path_edit = s;
                }
                Task::none()
            }
            Message::ElectrsDataBrowsed(p) => {
                if let Some(s) = p {
                    self.electrs_data_path_edit = s;
                }
                Task::none()
            }
            Message::SavePaths => self.save_paths(),
            Message::PathsSaved(result) => self.apply_paths_saved(result),
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
            Message::InstalledVersionsLoaded(versions) => {
                self.binary_page.installed_load = InventoryLoad::Idle;
                self.binary_page.installed_versions = Some(versions);
                Task::none()
            }
            Message::AvailableVersionsLoaded(versions) => {
                self.apply_available_versions(versions);
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
            Message::BuildFinished(result) => self.apply_build_finished(result),
            Message::CancelBuild => {
                if self.build_service.cancel_current() {
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
                if let Ok(info) = result.blockchain_info {
                    self.block_height = info.blocks;
                    self.bitcoin_synced = info.headers > 0
                        && info.blocks >= info.headers.saturating_sub(1)
                        && info.verification_progress > 0.9999;
                } else if !self.bitcoin_running {
                    self.block_height = 0;
                }
                self.apply_electrs_status(result.electrs_status);
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

        while let Ok(event) = self.build_event_rx.try_recv() {
            build_new |= matches!(event, BuildEvent::Log(_));
            self.apply_build_event(event);
        }

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

        if let Some(h) = &mut self.bitcoin_handle {
            if !h.is_running() {
                self.bitcoin_handle = None;
                self.bitcoin_running = false;
                self.bitcoin_synced = false;
                self.block_height = 0;
                self.electrs_status.synced = false;
                push_msg(&self.bitcoin_queue, "bitcoind has stopped.");
            }
        }

        if let Some(h) = &mut self.electrs_handle {
            if !h.is_running() {
                self.electrs_handle = None;
                self.electrs_status.running = false;
                self.electrs_status.synced = false;
                self.electrs_status.ready = false;
                push_msg(&self.electrs_queue, "electrs has stopped.");
            }
        }

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

        if tasks.is_empty() {
            Task::none()
        } else {
            Task::batch(tasks)
        }
    }

    fn handle_rpc_tick(&self) -> Task<Message> {
        let auth = RpcAuth::from_data_dir(&self.config.bitcoin_data_path);
        let config = self.config.clone();
        let process_running = self.electrs_handle.is_some();

        Task::perform(
            async move {
                let (blockchain_info, electrs_status) = tokio::join!(
                    async {
                        rpc::get_blockchain_info(&auth)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    electrs_status::probe(&config, process_running),
                );

                StatusPollResult {
                    blockchain_info,
                    electrs_status,
                }
            },
            Message::StatusPollReceived,
        )
    }

    fn save_paths(&mut self) -> Task<Message> {
        let bins = self.binaries_path_edit.trim().to_owned();
        let btc = self.bitcoin_data_path_edit.trim().to_owned();
        let els = self.electrs_data_path_edit.trim().to_owned();

        if bins.is_empty() || btc.is_empty() || els.is_empty() {
            self.overlay_message = Some("All path fields must be filled in.".into());
            return Task::none();
        }

        self.config.binaries_path = PathBuf::from(&bins);
        self.config.bitcoin_data_path = PathBuf::from(&btc);
        self.config.electrs_data_path = PathBuf::from(&els);

        let config_clone = self.config.clone();
        let btc_q = Arc::clone(&self.bitcoin_queue);
        let els_q = Arc::clone(&self.electrs_queue);

        Task::perform(
            async move {
                config_clone.save().map_err(|e| e.to_string())?;
                std::fs::create_dir_all(&config_clone.bitcoin_data_path)
                    .map_err(|e| e.to_string())?;
                std::fs::create_dir_all(&config_clone.electrs_data_path)
                    .map_err(|e| e.to_string())?;
                push_msg(&btc_q, "--- Paths updated ---");
                push_msg(&btc_q, &format!("Binaries : {bins}"));
                push_msg(&btc_q, &format!("Data dir : {btc}"));
                push_msg(&els_q, "--- Paths updated ---");
                push_msg(&els_q, &format!("DB dir   : {els}"));
                Ok(())
            },
            Message::PathsSaved,
        )
    }

    fn apply_paths_saved(&mut self, result: Result<(), String>) -> Task<Message> {
        match result {
            Ok(()) => {
                self.overlay_message = Some(format!(
                    "Paths saved.\nChanges take effect on the next node launch.\n\nConfig: {}",
                    Config::config_file_path().display()
                ));
            }
            Err(e) => {
                self.overlay_message = Some(format!("Failed to save paths:\n{e}"));
            }
        }
        Task::none()
    }

    fn launch_bitcoin(&mut self) -> Task<Message> {
        if self.bitcoin_running {
            self.overlay_message = Some("Bitcoin is already running.".into());
            return Task::none();
        }
        if let Err(e) = rpc::ensure_bitcoin_conf(&self.config.bitcoin_data_path) {
            self.overlay_message = Some(format!("Failed to prepare Bitcoin config:\n{e}"));
            return Task::none();
        }

        match process_manager::launch_bitcoind(
            &self.config.binaries_path,
            &self.config.bitcoin_data_path,
            &self.bitcoin_queue,
        ) {
            Ok(handle) => {
                self.bitcoin_handle = Some(handle);
                self.bitcoin_running = true;
                self.bitcoin_synced = false;
            }
            Err(e) => {
                push_msg(&self.bitcoin_queue, &format!("Launch error: {e}"));
                self.overlay_message = Some(format!("Failed to launch Bitcoin:\n{e}"));
            }
        }
        Task::none()
    }

    fn launch_electrs(&mut self) -> Task<Message> {
        if self.electrs_status.running {
            self.overlay_message = Some("Electrs is already running.".into());
            return Task::none();
        }
        if !self.bitcoin_running {
            self.overlay_message = Some(
                "Bitcoin must be running before starting Electrs.\n\
                 Launch Bitcoin first and wait for the Running indicator."
                    .into(),
            );
            return Task::none();
        }

        match process_manager::launch_electrs(
            &self.config.binaries_path,
            &self.config.bitcoin_data_path,
            &self.config.electrs_data_path,
            Config::electrum_addr(),
            &self.electrs_queue,
        ) {
            Ok(handle) => {
                self.electrs_handle = Some(handle);
                self.electrs_status = ElectrsStatus {
                    running: true,
                    ..ElectrsStatus::default()
                };
            }
            Err(e) => {
                push_msg(&self.electrs_queue, &format!("Launch error: {e}"));
                self.overlay_message = Some(format!("Failed to launch Electrs:\n{e}"));
            }
        }
        Task::none()
    }

    fn shutdown_both(&mut self) -> Task<Message> {
        self.terminate_electrs_internal();

        if self.bitcoin_running {
            let auth = RpcAuth::from_data_dir(&self.config.bitcoin_data_path);
            let btc_q = Arc::clone(&self.bitcoin_queue);
            push_msg(&btc_q, "Sending stop via RPC…");

            if let Some(mut handle) = self.bitcoin_handle.take() {
                self.bitcoin_running = false;
                self.bitcoin_synced = false;
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Handle::try_current();
                    let stopped_via_rpc = rt.map_or_else(
                        |_| {
                            tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .is_ok_and(|r| r.block_on(rpc::stop_bitcoind(&auth)).is_ok())
                        },
                        |rt| rt.block_on(rpc::stop_bitcoind(&auth)).is_ok(),
                    );
                    if stopped_via_rpc {
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(60);
                        loop {
                            if std::time::Instant::now() >= deadline {
                                handle.terminate();
                                break;
                            }
                            if !handle.is_running() {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    } else {
                        handle.terminate();
                    }
                    push_msg(&btc_q, "bitcoind stopped.");
                });
            }
        }

        Task::none()
    }

    fn shutdown_electrs_only(&mut self) -> Task<Message> {
        self.terminate_electrs_internal();
        Task::none()
    }

    fn refresh_binary_info(&mut self) -> Task<Message> {
        self.binary_page.installed_load = InventoryLoad::Loading;
        self.binary_page.available_load = InventoryLoad::Loading;
        let binaries_dir = self.config.binaries_path.clone();
        Task::batch([
            Task::perform(
                async move { InstalledVersions::detect(&binaries_dir).await },
                Message::InstalledVersionsLoaded,
            ),
            Task::perform(AvailableVersions::fetch(), Message::AvailableVersionsLoaded),
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
        if self.binary_page.active_kind.is_some() {
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

        self.binary_page.active_kind = Some(kind);
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
            kind,
            version,
            binaries_dir: self.config.binaries_path.clone(),
            workspace: binaries::workspace_for(&self.config.binaries_path),
            cores: build_worker_count(),
        };
        Task::perform(
            async move { service.run(request, event_tx).await },
            Message::BuildFinished,
        )
    }

    fn apply_build_event(&mut self, event: BuildEvent) {
        match event {
            BuildEvent::Stage { kind, stage } => {
                self.binary_page.active_kind = (stage != BuildStage::Complete).then_some(kind);
                self.binary_page.stage = Some(stage);
                self.binary_page.progress = stage.progress();
            }
            BuildEvent::Progress(progress) => {
                self.binary_page.progress = progress.clamp(0.0, 1.0);
            }
            BuildEvent::Log(message) => {
                self.binary_page
                    .log_lines
                    .extend(message.lines().map(ToOwned::to_owned));
            }
        }
    }

    fn apply_build_finished(
        &mut self,
        result: Result<BuildSummary, BuildFailure>,
    ) -> Task<Message> {
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
        if let Some(mut handle) = self.electrs_handle.take() {
            push_msg(&self.electrs_queue, "Terminating electrs…");
            let els_q = Arc::clone(&self.electrs_queue);
            std::thread::spawn(move || {
                handle.terminate();
                push_msg(&els_q, "electrs stopped.");
            });
        }
        self.electrs_status = ElectrsStatus::default();
    }

    fn apply_electrs_status(&mut self, mut next_status: ElectrsStatus) {
        let previous = self.electrs_status.clone();
        let warnings_ready = self.bitcoin_running && self.bitcoin_synced && next_status.running;

        if !warnings_ready {
            next_status.metrics_error = None;
            next_status.bitcoin_error = None;
            next_status.connect_error = None;
        }

        if warnings_ready {
            if previous.metrics_error != next_status.metrics_error {
                if let Some(error) = next_status.metrics_error.as_deref() {
                    push_msg(
                        &self.electrs_queue,
                        &format!("Electrs metrics check failed: {error}"),
                    );
                }
            }

            if previous.bitcoin_error != next_status.bitcoin_error {
                if let Some(error) = next_status.bitcoin_error.as_deref() {
                    push_msg(
                        &self.electrs_queue,
                        &format!("Electrs sync check failed: {error}"),
                    );
                }
            }

            if previous.connect_error != next_status.connect_error {
                if let Some(error) = next_status.connect_error.as_deref() {
                    push_msg(
                        &self.electrs_queue,
                        &format!("Electrs connectivity check failed: {error}"),
                    );
                }
            }
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

// ── Async helpers ─────────────────────────────────────────────────────────────

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

fn build_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |count| count.get().saturating_sub(1).clamp(1, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app(root: &Path) -> App {
        App::from_config(Config::defaults(root), None, root.join("build-state.json"))
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
    fn build_events_update_human_readable_status_and_log() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut app = test_app(temporary.path());
        app.apply_build_event(BuildEvent::Stage {
            kind: BinaryKind::Electrs,
            stage: BuildStage::Compiling,
        });
        app.apply_build_event(BuildEvent::Progress(0.7));
        app.apply_build_event(BuildEvent::Log("Compiling crate\nFinished\n".to_owned()));
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
        app.binary_page.active_kind = Some(BinaryKind::BitcoinCore);
        drop(app.apply_build_finished(Err(BuildFailure {
            message:
                "compiler exited with status 2; installed binaries were not changed".to_owned(),
            cancelled: false,
            conflict: false,
        })));
        assert_eq!(app.binary_page.active_kind, None);
        assert!(app
            .binary_page
            .error
            .as_deref()
            .is_some_and(|message| message.contains("not changed")));
        Ok(())
    }
}
