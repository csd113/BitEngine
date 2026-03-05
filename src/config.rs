//! Application configuration.
//!
//! Stored as JSON in `~/Library/Application Support/BitcoinNodeManager/config.json`
//! (macOS) or `~/.config/BitcoinNodeManager/config.json` (other Unix).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "BitcoinNodeManager";
const CONFIG_FILENAME: &str = "config.json";

/// All persisted settings for the node manager.
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
    pub fn load(ssd_root: &Path) -> Self {
        let defaults = Self::defaults(ssd_root);
        let path = Self::config_file_path();

        match Self::load_from_file(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("Config load error ({e}), using defaults.");
                defaults
            }
        }
    }

    /// Persist the current config to disk.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_file_path();
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {:?}", parent))?;
        }
        let json = serde_json::to_string_pretty(self).context("serialise config")?;
        std::fs::write(&path, json).with_context(|| format!("write config {:?}", path))?;
        Ok(())
    }

    /// Path to the JSON config file on this platform.
    pub fn config_file_path() -> PathBuf {
        if let Some(proj) = ProjectDirs::from("", "", APP_NAME) {
            proj.config_dir().join(CONFIG_FILENAME)
        } else {
            // Fallback
            dirs_fallback().join(CONFIG_FILENAME)
        }
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
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read config {:?}", path))?;
        serde_json::from_str(&text).context("parse config JSON")
    }
}

fn dirs_fallback() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join(APP_NAME)
}
