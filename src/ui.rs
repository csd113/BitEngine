//! Iced application — all UI state, messages, view logic, and update handlers.
//!
//! Architecture overview
//! ─────────────────────
//! The app follows the Elm/MVU pattern enforced by Iced 0.13:
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

use std::{path::PathBuf, sync::Arc, time::Duration};

use iced::widget::scrollable::{Direction, Id as ScrollId, Scrollbar};
use iced::{
    font::Font,
    time,
    widget::{button, column, container, row, scrollable, text, text_input, Space},
    Alignment, Color, Element, Length, Padding, Subscription, Task,
};

use crate::{
    config::Config,
    process_manager::{self, is_electrs_synced_line, new_queue, OutputQueue, ProcessHandle},
    rpc::{self, BlockchainInfo, RpcAuth},
    updater::{self, UpdateResult},
};

// ── Colour palette ────────────────────────────────────────────────────────────

const BG: Color = Color {
    r: 0.949,
    g: 0.949,
    b: 0.969,
    a: 1.0,
}; // #f2f2f7
const PANEL: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
}; // white
const BAR: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
}; // white
const BORDER: Color = Color {
    r: 0.820,
    g: 0.820,
    b: 0.839,
    a: 1.0,
}; // #d1d1d6
const TERM_BG: Color = Color {
    r: 0.118,
    g: 0.118,
    b: 0.118,
    a: 1.0,
}; // #1e1e1e
const TERM_FG: Color = Color {
    r: 0.831,
    g: 0.831,
    b: 0.831,
    a: 1.0,
}; // #d4d4d4
const GREEN: Color = Color {
    r: 0.204,
    g: 0.780,
    b: 0.349,
    a: 1.0,
}; // #34c759
const OFF: Color = Color {
    r: 0.820,
    g: 0.820,
    b: 0.839,
    a: 1.0,
}; // #d1d1d6
const MAC_BLUE: Color = Color {
    r: 0.0,
    g: 0.478,
    b: 1.0,
    a: 1.0,
}; // #007aff
const MAC_RED: Color = Color {
    r: 1.0,
    g: 0.231,
    b: 0.188,
    a: 1.0,
}; // #ff3b30
const MAC_ORG: Color = Color {
    r: 1.0,
    g: 0.584,
    b: 0.0,
    a: 1.0,
}; // #ff9500
const BTC_ACC: Color = Color {
    r: 0.973,
    g: 0.580,
    b: 0.102,
    a: 1.0,
}; // #f7931a
const ELS_ACC: Color = Color {
    r: 0.345,
    g: 0.337,
    b: 0.839,
    a: 1.0,
}; // #5856d6
const TEXT_SEC: Color = Color {
    r: 0.282,
    g: 0.282,
    b: 0.290,
    a: 1.0,
}; // #48484a
const TEXT_TER: Color = Color {
    r: 0.557,
    g: 0.557,
    b: 0.576,
    a: 1.0,
}; // #8e8e93

// ── Scrollable IDs for programmatic scroll-to-bottom ─────────────────────────

fn bitcoin_scroll_id() -> ScrollId {
    ScrollId::new("bitcoin_terminal")
}
fn electrs_scroll_id() -> ScrollId {
    ScrollId::new("electrs_terminal")
}

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

    // ── Async results ─────────────────────────────────────────────────────────
    BlockchainInfoReceived(Result<BlockchainInfo, String>),
    UpdateBinaries,
    UpdateResult(String), // human-readable outcome message

    // ── Modal / overlay ───────────────────────────────────────────────────────
    /// Dismiss the info/error overlay.
    DismissOverlay,
    /// Open BitForge.app (update flow).
    OpenBitForge(PathBuf),

    // ── No-op (used to complete Tasks that return nothing useful) ─────────────
    Noop,
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
    electrs_running: bool,
    electrs_synced: bool,
    block_height: u64,

    // ── UI state ──────────────────────────────────────────────────────────────
    paths_visible: bool,

    /// Non-empty ⇒ display an overlay dialog with this message.
    overlay_message: Option<String>,
    /// When `overlay_message` is set, this optional path allows a "Open BitForge" button.
    bitforge_path: Option<PathBuf>,
}

impl App {
    pub fn new(ssd_root: PathBuf) -> Self {
        let config = Config::load(&ssd_root);

        let binaries_edit = config.binaries_path.to_string_lossy().into_owned();
        let bitcoin_data_edit = config.bitcoin_data_path.to_string_lossy().into_owned();
        let electrs_data_edit = config.electrs_data_path.to_string_lossy().into_owned();

        let bitcoin_queue = new_queue();
        let electrs_queue = new_queue();

        // Log startup info into the terminal queues
        push_msg(&bitcoin_queue, "=== Bitcoin Node Manager started ===");
        push_msg(
            &bitcoin_queue,
            &format!("Config   : {}", Config::config_file_path().display()),
        );
        push_msg(
            &bitcoin_queue,
            &format!("Binaries : {}", config.binaries_path.display()),
        );
        push_msg(
            &bitcoin_queue,
            &format!("Data dir : {}", config.bitcoin_data_path.display()),
        );
        push_msg(&electrs_queue, "=== Electrs Node Manager started ===");
        push_msg(
            &electrs_queue,
            &format!("Binaries : {}", config.binaries_path.display()),
        );
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
            electrs_running: false,
            electrs_synced: false,
            block_height: 0,
            paths_visible: true,
            overlay_message: None,
            bitforge_path: None,
        }
    }

    // ── update ────────────────────────────────────────────────────────────────

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            // ── Timer: drain output queues ────────────────────────────────────
            Message::OutputTick => {
                let mut btc_new = false;
                let mut els_new = false;

                // Bitcoin queue
                if let Ok(mut q) = self.bitcoin_queue.lock() {
                    while let Some(line) = q.pop_front() {
                        self.bitcoin_lines.push(line);
                        btc_new = true;
                    }
                }
                // Electrs queue
                if let Ok(mut q) = self.electrs_queue.lock() {
                    while let Some(line) = q.pop_front() {
                        // Check for electrs sync signals
                        if is_electrs_synced_line(&line) {
                            self.electrs_synced = true;
                        }
                        self.electrs_lines.push(line);
                        els_new = true;
                    }
                }

                // Trim terminal buffers to last 5 000 lines
                const MAX: usize = 5_000;
                if self.bitcoin_lines.len() > MAX {
                    let drain_to = self.bitcoin_lines.len() - MAX;
                    self.bitcoin_lines.drain(..drain_to);
                }
                if self.electrs_lines.len() > MAX {
                    let drain_to = self.electrs_lines.len() - MAX;
                    self.electrs_lines.drain(..drain_to);
                }

                // Check if processes have exited
                if self.bitcoin_running {
                    if let Some(h) = &mut self.bitcoin_handle {
                        if !h.is_running() {
                            self.bitcoin_running = false;
                            self.bitcoin_synced = false;
                            self.block_height = 0;
                            // If bitcoin died, electrs status is also invalid
                            self.electrs_synced = false;
                            push_msg(&self.bitcoin_queue, "bitcoind has stopped.");
                        }
                    }
                }
                if self.electrs_running {
                    if let Some(h) = &mut self.electrs_handle {
                        if !h.is_running() {
                            self.electrs_running = false;
                            self.electrs_synced = false;
                            push_msg(&self.electrs_queue, "electrs has stopped.");
                        }
                    }
                }

                // Scroll terminals to bottom if new content arrived.
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if btc_new {
                    tasks.push(
                        scrollable::scroll_to(
                            bitcoin_scroll_id(),
                            scrollable::AbsoluteOffset {
                                x: 0.0,
                                y: f32::MAX,
                            },
                        )
                        .map(|_: iced_runtime::Action<Message>| Message::Noop),
                    );
                }
                if els_new {
                    tasks.push(
                        scrollable::scroll_to(
                            electrs_scroll_id(),
                            scrollable::AbsoluteOffset {
                                x: 0.0,
                                y: f32::MAX,
                            },
                        )
                        .map(|_: iced_runtime::Action<Message>| Message::Noop),
                    );
                }
                if tasks.is_empty() {
                    Task::none()
                } else {
                    Task::batch(tasks)
                }
            }

            // ── Timer: RPC poll ───────────────────────────────────────────────
            Message::RpcTick => {
                if !self.bitcoin_running {
                    return Task::none();
                }
                let auth = RpcAuth::from_data_dir(&self.config.bitcoin_data_path);
                Task::perform(
                    async move {
                        rpc::get_blockchain_info(&auth)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::BlockchainInfoReceived,
                )
            }

            // ── RPC result ────────────────────────────────────────────────────
            Message::BlockchainInfoReceived(result) => {
                if let Ok(info) = result {
                    self.block_height = info.blocks;
                    self.bitcoin_synced = info.headers > 0
                        && info.blocks >= info.headers.saturating_sub(1)
                        && info.verification_progress > 0.9999;
                }
                Task::none()
            }

            // ── Path editing ──────────────────────────────────────────────────
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

            Message::SavePaths => {
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
                        // Ensure data directories exist
                        let _ = std::fs::create_dir_all(&config_clone.bitcoin_data_path);
                        let _ = std::fs::create_dir_all(&config_clone.electrs_data_path);
                        push_msg(&btc_q, "--- Paths updated ---");
                        push_msg(&btc_q, &format!("Binaries : {}", bins));
                        push_msg(&btc_q, &format!("Data dir : {}", btc));
                        push_msg(&els_q, "--- Paths updated ---");
                        push_msg(&els_q, &format!("DB dir   : {}", els));
                        Ok(())
                    },
                    Message::PathsSaved,
                )
            }

            Message::PathsSaved(result) => {
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

            Message::TogglePathsPanel => {
                self.paths_visible = !self.paths_visible;
                Task::none()
            }

            // ── Launch nodes ──────────────────────────────────────────────────
            Message::LaunchBitcoin => {
                if self.bitcoin_running {
                    self.overlay_message = Some("Bitcoin is already running.".into());
                    return Task::none();
                }
                // Ensure bitcoin.conf exists
                let _ = rpc::ensure_bitcoin_conf(&self.config.bitcoin_data_path);

                match process_manager::launch_bitcoind(
                    &self.config.binaries_path,
                    &self.config.bitcoin_data_path,
                    Arc::clone(&self.bitcoin_queue),
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

            Message::LaunchElectrs => {
                if self.electrs_running {
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
                    Arc::clone(&self.electrs_queue),
                ) {
                    Ok(handle) => {
                        self.electrs_handle = Some(handle);
                        self.electrs_running = true;
                        self.electrs_synced = false;
                    }
                    Err(e) => {
                        push_msg(&self.electrs_queue, &format!("Launch error: {e}"));
                        self.overlay_message = Some(format!("Failed to launch Electrs:\n{e}"));
                    }
                }
                Task::none()
            }

            // ── Shutdown ──────────────────────────────────────────────────────
            Message::ShutdownBoth => {
                self.terminate_electrs_internal();

                if self.bitcoin_running {
                    let auth = RpcAuth::from_data_dir(&self.config.bitcoin_data_path);
                    let btc_q = Arc::clone(&self.bitcoin_queue);
                    push_msg(&btc_q, "Sending stop via RPC…");

                    // Move child out so we can wait on it in a background thread
                    if let Some(mut handle) = self.bitcoin_handle.take() {
                        self.bitcoin_running = false;
                        self.bitcoin_synced = false;
                        std::thread::spawn(move || {
                            let rt = tokio::runtime::Handle::try_current();
                            // Stop via RPC; if that fails, SIGTERM the process
                            let stopped_via_rpc = if let Ok(rt) = rt {
                                rt.block_on(rpc::stop_bitcoind(&auth)).is_ok()
                            } else {
                                // Fallback: build a mini runtime
                                tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                    .map(|r| r.block_on(rpc::stop_bitcoind(&auth)).is_ok())
                                    .unwrap_or(false)
                            };
                            if !stopped_via_rpc {
                                handle.terminate();
                            } else {
                                // Wait up to 60 s for graceful shutdown
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
                            }
                            push_msg(&btc_q, "bitcoind stopped.");
                        });
                    }
                }
                Task::none()
            }

            Message::ShutdownElectrsOnly => {
                self.terminate_electrs_internal();
                Task::none()
            }

            // ── Binary update ─────────────────────────────────────────────────
            Message::UpdateBinaries => {
                let binaries_dst = self.config.binaries_path.clone();
                let btc_q = Arc::clone(&self.bitcoin_queue);
                Task::perform(
                    async move {
                        let result = updater::run_update(&binaries_dst);
                        match result {
                            UpdateResult::Updated(msg) => {
                                push_msg(&btc_q, &format!("Update complete: {msg}"));
                                format!("Successfully updated:\n\n{msg}")
                            }
                            UpdateResult::BitForgeFound(path) => {
                                format!("__BITFORGE_FOUND__{}", path.display())
                            }
                            UpdateResult::BitForgeNotFound => {
                                "No bitcoin_builds folder found.\n\n\
                                 Download BitForge from:\n\
                                 https://github.com/csd113/BitForge-Python".into()
                            }
                            UpdateResult::BinariesSubfolderMissing => {
                                "Found ~/Downloads/bitcoin_builds but no 'binaries/' sub-folder inside it.".into()
                            }
                            UpdateResult::NothingToUpdate => {
                                "No bitcoin-X.Y.Z or electrs-X.Y.Z folders found in the binaries folder.".into()
                            }
                        }
                    },
                    Message::UpdateResult,
                )
            }

            Message::UpdateResult(msg) => {
                // Special sentinel: BitForge was found
                if let Some(path_str) = msg.strip_prefix("__BITFORGE_FOUND__") {
                    self.bitforge_path = Some(PathBuf::from(path_str));
                    self.overlay_message = Some(
                        "No bitcoin_builds folder found.\n\nBitForge.app is installed — open it to build binaries?".into()
                    );
                } else {
                    self.bitforge_path = None;
                    self.overlay_message = Some(msg);
                }
                Task::none()
            }

            Message::DismissOverlay => {
                self.overlay_message = None;
                self.bitforge_path = None;
                Task::none()
            }

            Message::OpenBitForge(path) => {
                let _ = std::process::Command::new("open").arg(&path).spawn();
                self.overlay_message = None;
                self.bitforge_path = None;
                Task::none()
            }

            Message::Noop => Task::none(),
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
        self.electrs_running = false;
        self.electrs_synced = false;
    }

    // ── subscription ──────────────────────────────────────────────────────────

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            time::every(Duration::from_millis(100)).map(|_| Message::OutputTick),
            time::every(Duration::from_secs(5)).map(|_| Message::RpcTick),
        ])
    }

    // ── view ──────────────────────────────────────────────────────────────────

    pub fn view(&self) -> Element<'_, Message> {
        let content = column![
            self.view_toolbar(),
            horizontal_rule(),
            self.view_paths_panel(),
            self.view_node_panels(),
            horizontal_rule(),
            self.view_bottom_bar(),
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        // Overlay dialog (modal-like)
        if let Some(msg) = &self.overlay_message {
            view_overlay(msg, self.bitforge_path.clone())
        } else {
            container(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_| container::Style {
                    background: Some(BG.into()),
                    ..Default::default()
                })
                .into()
        }
    }

    // ── Toolbar ───────────────────────────────────────────────────────────────

    fn view_toolbar(&self) -> Element<'_, Message> {
        let height_text: String = if self.block_height > 0 {
            // Format with thousands separators: 895234 → "895,234"
            let s = self.block_height.to_string();
            let mut out = String::with_capacity(s.len() + s.len() / 3);
            for (i, ch) in s.chars().rev().enumerate() {
                if i > 0 && i % 3 == 0 {
                    out.push(',');
                }
                out.push(ch);
            }
            out.chars().rev().collect::<String>()
        } else {
            "Connecting…".to_owned()
        };

        let block_stat = column![
            text("BLOCK HEIGHT").size(9).color(TEXT_TER),
            text(height_text)
                .size(18)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
        ]
        .spacing(2);

        let update_btn = styled_button("Update Binaries…", ButtonStyle::Secondary)
            .on_press(Message::UpdateBinaries);

        let toolbar_row = row![block_stat, Space::with_width(Length::Fill), update_btn,]
            .align_y(Alignment::Center)
            .padding(Padding::from([0, 16]));

        container(toolbar_row)
            .width(Length::Fill)
            .height(56)
            .style(|_| container::Style {
                background: Some(BAR.into()),
                ..Default::default()
            })
            .into()
    }

    // ── Paths panel ───────────────────────────────────────────────────────────

    fn view_paths_panel(&self) -> Element<'_, Message> {
        let toggle_label = if self.paths_visible { "Hide" } else { "Show" };

        let header = row![
            text("DIRECTORY PATHS").size(10).color(TEXT_TER),
            text(format!(
                "  Config: {}",
                Config::config_file_path().display()
            ))
            .size(9)
            .color(TEXT_TER),
            Space::with_width(Length::Fill),
            styled_button(toggle_label, ButtonStyle::Secondary).on_press(Message::TogglePathsPanel),
        ]
        .align_y(Alignment::Center)
        .padding(Padding::from([10, 20]));

        if !self.paths_visible {
            return container(header)
                .width(Length::Fill)
                .style(|_| container::Style {
                    background: Some(BAR.into()),
                    ..Default::default()
                })
                .into();
        }

        let rows = column![
            path_row(
                "Binaries Folder",
                &self.binaries_path_edit,
                Message::BinariesPathChanged,
                Message::BrowseBinaries,
                std::path::Path::new(&self.binaries_path_edit).exists(),
            ),
            path_row(
                "Bitcoin Data Directory",
                &self.bitcoin_data_path_edit,
                Message::BitcoinDataPathChanged,
                Message::BrowseBitcoinData,
                std::path::Path::new(&self.bitcoin_data_path_edit).exists(),
            ),
            path_row(
                "Electrs DB Directory",
                &self.electrs_data_path_edit,
                Message::ElectrsDataPathChanged,
                Message::BrowseElectrsData,
                std::path::Path::new(&self.electrs_data_path_edit).exists(),
            ),
            row![
                text("Changes take effect on the next node launch.")
                    .size(10)
                    .color(TEXT_TER),
                Space::with_width(Length::Fill),
                styled_button("Save Paths", ButtonStyle::Confirm).on_press(Message::SavePaths),
            ]
            .align_y(Alignment::Center)
            .padding(Padding::from([8, 0])),
        ]
        .spacing(4)
        .padding(Padding::from([0, 20]));

        let body = column![header, rows].padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 4.0,
            left: 0.0,
        });

        container(body)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(BAR.into()),
                ..Default::default()
            })
            .into()
    }

    // ── Dual node panels ──────────────────────────────────────────────────────

    fn view_node_panels(&self) -> Element<'_, Message> {
        let bitcoin_panel = self.view_node_panel(
            "Bitcoin",
            BTC_ACC,
            Message::LaunchBitcoin,
            self.bitcoin_running,
            self.bitcoin_synced,
            self.bitcoin_running && self.bitcoin_synced,
            &self.bitcoin_lines,
            bitcoin_scroll_id(),
        );
        let electrs_panel = self.view_node_panel(
            "Electrs",
            ELS_ACC,
            Message::LaunchElectrs,
            self.electrs_running,
            self.electrs_synced,
            self.electrs_running && self.electrs_synced,
            &self.electrs_lines,
            electrs_scroll_id(),
        );

        row![bitcoin_panel, electrs_panel]
            .spacing(0)
            .height(Length::Fill)
            .into()
    }

    #[allow(clippy::too_many_arguments)]
    fn view_node_panel<'a>(
        &'a self,
        title: &'a str,
        accent: Color,
        launch_msg: Message,
        running: bool,
        synced: bool,
        ready: bool,
        lines: &'a [String],
        scroll_id: ScrollId,
    ) -> Element<'a, Message> {
        // Accent top bar (3 px)
        let accent_bar = container(Space::with_height(3))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(accent.into()),
                ..Default::default()
            });

        // Header row: title + Launch button
        let launch_btn = button(
            text("Launch")
                .size(13)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::WHITE),
        )
        .padding(Padding::from([5, 18]))
        .style(move |_, status| button::Style {
            background: Some(match status {
                button::Status::Hovered | button::Status::Pressed => darken(accent).into(),
                _ => accent.into(),
            }),
            text_color: Color::WHITE,
            border: iced::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 6.0.into(),
            },
            shadow: iced::Shadow::default(),
        })
        .on_press(launch_msg);

        let header = row![
            text(title)
                .size(20)
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .color(Color::BLACK),
            Space::with_width(Length::Fill),
            launch_btn,
        ]
        .align_y(Alignment::Center)
        .padding(Padding {
            top: 14.0,
            right: 20.0,
            bottom: 10.0,
            left: 20.0,
        });

        // Indicators
        let indicators = row![
            indicator_badge("Running", running),
            Space::with_width(24),
            indicator_badge("Synced", synced),
            Space::with_width(24),
            indicator_badge("Ready", ready),
        ]
        .align_y(Alignment::Center)
        .padding(Padding::from([8, 20]));

        // Terminal
        let terminal_lines: Vec<Element<Message>> = lines
            .iter()
            .map(|l| {
                text(l.as_str())
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(TERM_FG)
                    .into()
            })
            .collect();

        let terminal_content = column(terminal_lines)
            .spacing(0)
            .width(Length::Fill)
            .padding(Padding::from([8, 10]));

        let terminal = scrollable(terminal_content)
            .id(scroll_id)
            .direction(Direction::Vertical(Scrollbar::default()))
            .height(Length::Fill)
            .width(Length::Fill);

        let terminal_container = container(terminal)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(TERM_BG.into()),
                ..Default::default()
            });

        let panel = column![
            accent_bar,
            header,
            horizontal_rule(),
            indicators,
            horizontal_rule(),
            terminal_container,
        ]
        .width(Length::Fill)
        .height(Length::Fill);

        container(panel)
            .width(Length::FillPortion(1))
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(PANEL.into()),
                border: iced::Border {
                    color: BORDER,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                ..Default::default()
            })
            .into()
    }

    // ── Bottom bar ────────────────────────────────────────────────────────────

    fn view_bottom_bar(&self) -> Element<'_, Message> {
        let shutdown_both = styled_button("Shutdown Bitcoind & Electrs", ButtonStyle::Destructive)
            .on_press(Message::ShutdownBoth);
        let shutdown_els = styled_button("Shutdown Electrs Only", ButtonStyle::Warning)
            .on_press(Message::ShutdownElectrsOnly);

        let btn_row = row![shutdown_both, Space::with_width(8), shutdown_els]
            .align_y(Alignment::Center)
            .padding(Padding::from([12, 16]));

        container(btn_row)
            .width(Length::Fill)
            .height(56)
            .style(|_| container::Style {
                background: Some(BAR.into()),
                ..Default::default()
            })
            .into()
    }
}

// ── Overlay (modal dialog) ────────────────────────────────────────────────────

fn view_overlay<'a>(message: &'a str, bitforge_path: Option<PathBuf>) -> Element<'a, Message> {
    let mut buttons: Vec<Element<Message>> = vec![styled_button("OK", ButtonStyle::Primary)
        .on_press(Message::DismissOverlay)
        .into()];

    if let Some(path) = bitforge_path {
        buttons.insert(
            0,
            styled_button("Open BitForge", ButtonStyle::Confirm)
                .on_press(Message::OpenBitForge(path))
                .into(),
        );
    }

    let dialog = container(
        column![
            text(message).size(14).color(Color::BLACK),
            Space::with_height(16),
            row(buttons).spacing(8).align_y(Alignment::Center),
        ]
        .spacing(0)
        .padding(24)
        .width(440),
    )
    .style(|_| container::Style {
        background: Some(Color::WHITE.into()),
        border: iced::Border {
            color: BORDER,
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: iced::Shadow {
            color: Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.25,
            },
            offset: iced::Vector { x: 0.0, y: 4.0 },
            blur_radius: 20.0,
        },
        ..Default::default()
    });

    let backdrop = container(dialog)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Alignment::Center)
        .align_y(Alignment::Center)
        .style(|_| container::Style {
            background: Some(
                Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.4,
                }
                .into(),
            ),
            ..Default::default()
        });

    backdrop.into()
}

// ── Widget helpers ────────────────────────────────────────────────────────────

fn horizontal_rule<'a>() -> Element<'a, Message> {
    container(Space::with_height(1))
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(BORDER.into()),
            ..Default::default()
        })
        .into()
}

fn indicator_badge<'a>(label: &'a str, active: bool) -> Element<'a, Message> {
    let dot_color = if active { GREEN } else { OFF };
    row![
        text("●").size(14).color(dot_color),
        Space::with_width(6),
        text(label).size(11).color(TEXT_SEC),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn path_row<'a>(
    label: &'a str,
    value: &'a str,
    on_change: impl Fn(String) -> Message + 'a,
    browse_msg: Message,
    exists: bool,
) -> Element<'a, Message> {
    let exists_dot = text("●").size(13).color(if exists { GREEN } else { OFF });

    row![
        text(label).size(11).color(TEXT_SEC).width(180),
        text_input("", value)
            .on_input(on_change)
            .padding(Padding::from([4, 6]))
            .font(Font::MONOSPACE)
            .size(11),
        Space::with_width(6),
        styled_button("Browse…", ButtonStyle::Secondary).on_press(browse_msg),
        Space::with_width(6),
        exists_dot,
    ]
    .align_y(Alignment::Center)
    .spacing(4)
    .padding(Padding::from([3, 0]))
    .into()
}

// ── Button styling ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ButtonStyle {
    Primary,
    Secondary,
    Destructive,
    Warning,
    Confirm,
}

fn styled_button(label: &str, style: ButtonStyle) -> button::Button<'_, Message> {
    let (bg, hover_bg, fg) = match style {
        ButtonStyle::Primary => (MAC_BLUE, darken(MAC_BLUE), Color::WHITE),
        ButtonStyle::Secondary => (
            Color {
                r: 0.898,
                g: 0.898,
                b: 0.918,
                a: 1.0,
            },
            Color {
                r: 0.847,
                g: 0.847,
                b: 0.871,
                a: 1.0,
            },
            Color::BLACK,
        ),
        ButtonStyle::Destructive => (MAC_RED, darken(MAC_RED), Color::WHITE),
        ButtonStyle::Warning => (MAC_ORG, darken(MAC_ORG), Color::WHITE),
        ButtonStyle::Confirm => (GREEN, darken(GREEN), Color::WHITE),
    };

    button(text(label).size(11).color(fg))
        .padding(Padding::from([5, 14]))
        .style(move |_, status| button::Style {
            background: Some(match status {
                button::Status::Hovered | button::Status::Pressed => hover_bg.into(),
                _ => bg.into(),
            }),
            text_color: fg,
            border: iced::Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 6.0.into(),
            },
            shadow: iced::Shadow::default(),
        })
}

// ── Colour utilities ──────────────────────────────────────────────────────────

fn darken(c: Color) -> Color {
    Color {
        r: (c.r * 0.85).min(1.0),
        g: (c.g * 0.85).min(1.0),
        b: (c.b * 0.85).min(1.0),
        a: c.a,
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
        if q.len() > 10_000 {
            q.pop_front();
        }
        q.push_back(msg.to_owned());
    }
}
