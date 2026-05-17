//! `BitEngine` application entry point.
//!
//! Entry point.  Responsibilities:
//!   1. Single-instance lock.
//!   2. Resolves the default working root.
//!   3. Hands off to the Iced application loop.

#![expect(
    clippy::multiple_crate_versions,
    reason = "Iced, rfd, and their platform backends currently pull duplicate transitive crate versions"
)]

mod config;
mod electrs_status;
mod platform;
mod process_manager;
mod rpc;
mod ui;
mod updater;

use std::{path::PathBuf, process};

use iced::{window, Size, Task};

fn main() -> iced::Result {
    // ── Single-instance guard ────────────────────────────────────────────────
    let Some(_lock) = platform::SingleInstanceGuard::acquire() else {
        // Another instance is already running — exit silently.
        process::exit(0);
    };

    // ── Resolve SSD / working root ───────────────────────────────────────────
    // The app binary lives at the root of the SSD.  When bundled as a .app,
    // the binary is inside Contents/MacOS/, so we walk up to the .app's
    // parent directory.
    let ssd_root = resolve_ssd_root();

    // ── Launch Iced application ──────────────────────────────────────────────
    iced::application(
        move || {
            let app = ui::App::new(&ssd_root);
            (app, Task::none())
        },
        ui::App::update,
        ui::App::view,
    )
    .title("BitEngine")
    .subscription(ui::App::subscription)
    .theme(app_theme)
    .window(window::Settings {
        size: Size::new(1440.0, 960.0),
        min_size: Some(Size::new(900.0, 700.0)),
        resizable: true,
        decorations: true,
        ..Default::default()
    })
    .run()
}

const fn app_theme(_: &ui::App) -> iced::Theme {
    iced::Theme::Dark
}

/// Determine the default working root directory.
///
/// Priority:
///   1. If `BITENGINE_ROOT` env var is set, use that.
///   2. If the legacy `BITCOIN_NODE_MANAGER_ROOT` env var is set, use that.
///   3. On macOS, if the binary is inside a `.app` bundle, walk up to the
///      bundle's parent.
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

    #[cfg(target_os = "macos")]
    {
        // If inside <something>.app/Contents/MacOS/
        // walk up three levels to get the .app's parent directory.
        if exe_dir.ends_with(std::path::Path::new("Contents/MacOS")) {
            if let Some(bundle_parent) = exe_dir
                .parent() // Contents/
                .and_then(|p| p.parent()) // <Name>.app/
                .and_then(|p| p.parent())
            // SSD root
            {
                return bundle_parent.to_path_buf();
            }
        }
    }

    exe_dir.to_path_buf()
}
