//! `BitEngine` application entry point.
//!
//! Entry point.  Responsibilities:
//!   1. Single-instance lock.
//!   2. Resolves the default working root.
//!   3. Hands off to the Iced application loop.

#![expect(
    clippy::multiple_crate_versions,
    reason = "Iced, rfd, Arti, and their platform backends currently pull duplicate transitive crate versions"
)]

mod binaries;
mod bitcoin_config;
mod bitcoin_status;
mod config;
mod connection;
mod electrs_status;
mod platform;
mod process_manager;
mod rpc;
mod tor;
mod ui;

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
            let detect_theme = iced::system::theme().map(ui::Message::SystemThemeChanged);
            let start_tor_manager = app.initial_task();
            (app, Task::batch([detect_theme, start_tor_manager]))
        },
        ui::App::update,
        ui::App::view,
    )
    .title("BitEngine")
    .subscription(ui::App::subscription)
    .theme(ui::App::theme)
    .window(window::Settings {
        size: Size::new(1440.0, 960.0),
        min_size: Some(Size::new(900.0, 700.0)),
        resizable: true,
        decorations: true,
        ..Default::default()
    })
    .exit_on_close_request(false)
    .run()
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
        if safe_root_override(&p) {
            return p;
        }
    }

    if let Ok(env_root) = std::env::var("BITCOIN_NODE_MANAGER_ROOT") {
        let p = PathBuf::from(env_root);
        if safe_root_override(&p) {
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

fn safe_root_override(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .any(|component| matches!(component, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_override_is_accepted_lexically_without_requiring_existence() {
        let missing = PathBuf::from("/definitely-not-mounted/bitengine-root");
        assert!(safe_root_override(&missing));
        assert!(!safe_root_override(std::path::Path::new("relative/root")));
        assert!(!safe_root_override(std::path::Path::new("/")));
    }
}
