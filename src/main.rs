//! `BitEngine` — macOS
//!
//! Entry point.  Responsibilities:
//!   1. Single-instance lock (prevents double-launch from macOS .app open events).
//!   2. Resolves the SSD root (directory containing this binary).
//!   3. Hands off to the Iced application loop.

mod config;
mod electrs_status;
mod process_manager;
mod rpc;
mod ui;
mod updater;

use std::{
    fs::{self, OpenOptions},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    process,
};

use iced::{window, Size, Task};

/// Attempt to acquire an exclusive advisory lock on a temp file.
/// Returns an open file handle on success (caller must keep it alive).
/// Returns `None` if another instance already holds the lock.
fn acquire_single_instance_lock() -> Option<fs::File> {
    use std::os::unix::io::AsRawFd as _;
    let lock_path = std::env::temp_dir().join("BitEngine.lock");

    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .ok()?;

    // SAFETY: `file` is a live file descriptor opened in this function and
    // `flock` only inspects that descriptor to acquire an advisory lock.
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Some(file)
    } else {
        None
    }
}

fn main() -> iced::Result {
    // ── Single-instance guard ────────────────────────────────────────────────
    // macOS can fire two consecutive "open" events for the same .app bundle,
    // causing the app to open and immediately close.  We hold an exclusive
    // flock() for the lifetime of the process.
    let Some(_lock) = acquire_single_instance_lock() else {
        // Another instance is already running — exit silently.
        process::exit(0);
    };

    // ── Resolve SSD / working root ───────────────────────────────────────────
    // The app binary lives at the root of the SSD.  When bundled as a .app,
    // the binary is inside Contents/MacOS/, so we walk up to the .app's
    // parent directory.
    let ssd_root = resolve_ssd_root();

    // ── Launch Iced application ──────────────────────────────────────────────
    iced::application("BitEngine", ui::App::update, ui::App::view)
        .subscription(ui::App::subscription)
        .theme(|_| iced::Theme::Dark)
        .window(window::Settings {
            size: Size::new(1440.0, 960.0),
            min_size: Some(Size::new(900.0, 700.0)),
            resizable: true,
            decorations: true,
            ..Default::default()
        })
        .run_with(move || {
            let app = ui::App::new(&ssd_root);
            (app, Task::none())
        })
}

/// Determine the SSD root directory.
///
/// Priority:
///   1. If `BITENGINE_ROOT` env var is set, use that.
///   2. If the legacy `BITCOIN_NODE_MANAGER_ROOT` env var is set, use that.
///   3. If the binary is inside a `.app` bundle, walk up to the bundle's parent.
///   4. Otherwise, use the directory containing the binary.
fn resolve_ssd_root() -> PathBuf {
    if let Ok(env_root) = std::env::var("BITENGINE_ROOT") {
        let p = PathBuf::from(env_root);
        if p.is_dir() {
            return p;
        }
    }

    if let Ok(env_root) = std::env::var("BITCOIN_NODE_MANAGER_ROOT") {
        let p = PathBuf::from(env_root);
        if p.is_dir() {
            return p;
        }
    }

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    let exe_dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));

    // If inside <something>.app/Contents/MacOS/
    // walk up three levels to get the .app's parent directory.
    if exe_dir.ends_with(Path::new("Contents/MacOS")) {
        if let Some(bundle_parent) = exe_dir
            .parent() // Contents/
            .and_then(|p| p.parent()) // <Name>.app/
            .and_then(|p| p.parent())
        // SSD root
        {
            return bundle_parent.to_path_buf();
        }
    }

    exe_dir.to_path_buf()
}

// libc is used for flock() in acquire_single_instance_lock()
