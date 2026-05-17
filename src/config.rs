//! Application configuration.
//!
//! Stored as JSON in the platform config directory resolved by `directories`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const LEGACY_APP_NAME: &str = "BitcoinNodeManager";
const CONFIG_FILENAME: &str = "config.json";
pub const DEFAULT_ELECTRS_METRICS_ADDR: &str = "127.0.0.1:4224";
pub const DEFAULT_ELECTRUM_ADDR: &str = "127.0.0.1:50001";

/// All persisted settings for the node manager.
#[expect(
    clippy::struct_field_names,
    reason = "persisted fields mirror the stored config keys"
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directory containing `bitcoind`, `bitcoin-cli`, `electrs`, etc.
    pub binaries_path: PathBuf,
    /// Bitcoin data directory (holds `bitcoin.conf`, chainstate, blocks).
    pub bitcoin_data_path: PathBuf,
    /// Electrs database directory.
    pub electrs_data_path: PathBuf,
}

impl Config {
    /// Load from disk, falling back to sensible defaults derived from `ssd_root`.
    pub fn load(ssd_root: &Path) -> (Self, Option<String>) {
        let defaults = Self::defaults(ssd_root);
        let path = Self::config_file_path();

        match Self::load_from_file(&path) {
            Ok(cfg) => (cfg, None),
            Err(primary_err) => {
                if matches!(
                    primary_err.downcast_ref::<std::io::Error>(),
                    Some(err) if err.kind() == std::io::ErrorKind::NotFound
                ) {
                    let legacy_path = Self::legacy_config_file_path();
                    if legacy_path.exists() {
                        return match Self::load_from_file(&legacy_path) {
                            Ok(cfg) => (cfg, None),
                            Err(legacy_err) => {
                                let warning =
                                    format!("Config load error ({legacy_err}), using defaults.");
                                (defaults, Some(warning))
                            }
                        };
                    }
                }

                let warning = format!("Config load error ({primary_err}), using defaults.");
                (defaults, Some(warning))
            }
        }
    }

    /// Persist the current config to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_file_path();
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialise config")?;
        std::fs::write(&path, json).with_context(|| format!("write config {}", path.display()))?;
        Ok(())
    }

    /// Path to the JSON config file on this platform.
    pub fn config_file_path() -> PathBuf {
        ProjectDirs::from("", "", crate::platform::APP_NAME).map_or_else(
            || dirs_fallback(crate::platform::APP_NAME).join(CONFIG_FILENAME),
            |proj| proj.config_dir().join(CONFIG_FILENAME),
        )
    }

    fn legacy_config_file_path() -> PathBuf {
        ProjectDirs::from("", "", LEGACY_APP_NAME).map_or_else(
            || dirs_fallback(LEGACY_APP_NAME).join(CONFIG_FILENAME),
            |proj| proj.config_dir().join(CONFIG_FILENAME),
        )
    }

    pub fn electrs_metrics_url() -> String {
        format!("http://{DEFAULT_ELECTRS_METRICS_ADDR}/metrics")
    }

    pub const fn electrum_addr() -> &'static str {
        DEFAULT_ELECTRUM_ADDR
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn defaults(ssd_root: &Path) -> Self {
        Self {
            binaries_path: ssd_root.join("Binaries"),
            bitcoin_data_path: ssd_root.join("BitcoinChain"),
            electrs_data_path: ssd_root.join("ElectrsDB"),
        }
    }

    fn load_from_file(path: &PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        serde_json::from_str(&text).context("parse config JSON")
    }
}

fn dirs_fallback(app_name: &str) -> PathBuf {
    crate::platform::home_dir().join(".config").join(app_name)
}
