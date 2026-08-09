//! Application configuration.
//!
//! Stored as JSON in the platform config directory resolved by `directories`.

use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const LEGACY_APP_NAME: &str = "BitcoinNodeManager";
const CONFIG_FILENAME: &str = "config.json";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
static TEMPORARY_FILE_ID: AtomicU64 = AtomicU64::new(0);
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
                    return match Self::load_from_file(&legacy_path) {
                        Ok(cfg) => (cfg, None),
                        Err(legacy_err)
                            if matches!(
                                legacy_err.downcast_ref::<std::io::Error>(),
                                Some(err) if err.kind() == std::io::ErrorKind::NotFound
                            ) =>
                        {
                            (defaults, None)
                        }
                        Err(legacy_err) => {
                            let warning =
                                format!("Config load error ({legacy_err}), using defaults.");
                            (defaults, Some(warning))
                        }
                    };
                }

                let warning = format!("Config load error ({primary_err}), using defaults.");
                (defaults, Some(warning))
            }
        }
    }

    /// Persist the current config to disk.
    pub fn save(&self) -> Result<()> {
        self.validate_paths()?;
        let path = Self::config_file_path();
        if let Some(parent) = path.parent() {
            prepare_config_directory(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).context("serialise config")?;
        write_atomically(&path, &json)
    }

    /// Validate persisted paths before they can drive directory creation,
    /// source cleanup, or binary installation.
    pub fn validate_paths(&self) -> Result<()> {
        for (label, path) in [
            ("binaries", &self.binaries_path),
            ("Bitcoin data", &self.bitcoin_data_path),
            ("electrs data", &self.electrs_data_path),
        ] {
            validate_directory_path(label, path)?;
        }

        let workspace = crate::binaries::workspace_for(&self.binaries_path);
        validate_directory_path("build workspace", &workspace)?;
        validate_disjoint_paths(&[
            ("binaries", self.binaries_path.as_path()),
            ("build workspace", workspace.as_path()),
            ("Bitcoin data", self.bitcoin_data_path.as_path()),
            ("electrs data", self.electrs_data_path.as_path()),
        ])?;

        let resolved_binaries = resolve_for_comparison(&self.binaries_path)?;
        let resolved_bitcoin_data = resolve_for_comparison(&self.bitcoin_data_path)?;
        let resolved_electrs_data = resolve_for_comparison(&self.electrs_data_path)?;
        let resolved_workspace = resolve_for_comparison(&workspace)?;
        validate_resolved_disjoint_paths(&[
            ("binaries", resolved_binaries.as_path()),
            ("build workspace", resolved_workspace.as_path()),
            ("Bitcoin data", resolved_bitcoin_data.as_path()),
            ("electrs data", resolved_electrs_data.as_path()),
        ])
    }

    /// Path to the JSON config file on this platform.
    pub fn config_file_path() -> PathBuf {
        ProjectDirs::from("", "", crate::platform::APP_NAME).map_or_else(
            || dirs_fallback(crate::platform::APP_NAME).join(CONFIG_FILENAME),
            |proj| proj.config_dir().join(CONFIG_FILENAME),
        )
    }

    /// Durable state for the most recent native binary build.
    pub fn build_state_file_path() -> PathBuf {
        Self::config_file_path().with_file_name("build-job.json")
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

    pub(crate) fn defaults(ssd_root: &Path) -> Self {
        Self {
            binaries_path: ssd_root.join("Binaries"),
            bitcoin_data_path: ssd_root.join("BitcoinChain"),
            electrs_data_path: ssd_root.join("ElectrsDB"),
        }
    }

    fn load_from_file(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt as _;

        let initial_metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect config {}", path.display()))?;
        if initial_metadata.file_type().is_symlink()
            || !initial_metadata.is_file()
            || initial_metadata.len() > MAX_CONFIG_BYTES
        {
            anyhow::bail!("config is not a bounded regular file: {}", path.display());
        }

        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let file = options
            .open(path)
            .with_context(|| format!("open config {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect config {}", path.display()))?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            anyhow::bail!("config is not a bounded regular file: {}", path.display());
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        file.take(MAX_CONFIG_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("read config {}", path.display()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
            anyhow::bail!("config is unexpectedly large: {}", path.display());
        }
        let config = serde_json::from_slice::<Self>(&bytes).context("parse config JSON")?;
        config.validate_paths()?;
        Ok(config)
    }
}

fn validate_directory_path(label: &str, path: &Path) -> Result<()> {
    if !path.is_absolute() {
        anyhow::bail!("{label} path must be absolute: {}", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!("{label} path must not contain . or ..: {}", path.display());
    }
    if path
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
        < 2
    {
        anyhow::bail!(
            "{label} path is too close to the filesystem root: {}",
            path.display()
        );
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!(
                "{label} path must not be a symbolic link: {}",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_dir() => {
            anyhow::bail!("{label} path is not a directory: {}", path.display());
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {label} path {}", path.display()))
        }
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn validate_disjoint_paths(paths: &[(&str, &Path)]) -> Result<()> {
    for (index, (left_label, left)) in paths.iter().enumerate() {
        for (right_label, right) in &paths[index + 1..] {
            if paths_overlap(left, right) {
                anyhow::bail!(
                    "{left_label} and {right_label} paths must not overlap ({} and {})",
                    left.display(),
                    right.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_resolved_disjoint_paths(paths: &[(&str, &Path)]) -> Result<()> {
    for (index, (left_label, left)) in paths.iter().enumerate() {
        for (right_label, right) in &paths[index + 1..] {
            if paths_overlap(left, right) {
                anyhow::bail!(
                    "{left_label} and {right_label} paths resolve to overlapping locations ({} and {})",
                    left.display(),
                    right.display()
                );
            }
        }
    }
    Ok(())
}

fn resolve_for_comparison(path: &Path) -> Result<PathBuf> {
    let mut ancestor = path;
    let mut missing_components = Vec::<OsString>::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                for component in missing_components.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor
                    .file_name()
                    .with_context(|| format!("resolve configured path {}", path.display()))?;
                missing_components.push(component.to_os_string());
                ancestor = ancestor
                    .parent()
                    .with_context(|| format!("resolve configured path {}", path.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolve configured path {}", path.display()));
            }
        }
    }
}

fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let (temporary, mut file) = create_temporary_file(path)?;
    let result = (|| {
        file.write_all(contents)
            .with_context(|| format!("write temporary config {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary config {}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, path)
            .with_context(|| format!("activate config {}", path.display()))?;
        sync_parent_directory(path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn prepare_config_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create config directory {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect config directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("unsafe config directory: {}", path.display());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("set config directory permissions {}", path.display()))?;
    }
    Ok(())
}

fn create_temporary_file(path: &Path) -> Result<(PathBuf, File)> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    let file_name = path
        .file_name()
        .with_context(|| format!("config path has no file name: {}", path.display()))?;
    for _ in 0..64 {
        let identifier = TEMPORARY_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = file_name.to_os_string();
        temporary_name.push(format!(".tmp.{}.{}", std::process::id(), identifier));
        let temporary = path.with_file_name(temporary_name);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create temporary config {}", temporary.display()));
            }
        }
    }
    anyhow::bail!(
        "could not allocate a unique temporary file for {}",
        path.display()
    )
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("config path has no parent: {}", path.display()))?;
    File::open(parent)
        .with_context(|| format!("open config directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("sync config directory {}", parent.display()))
}

#[cfg(not(unix))]
const fn sync_parent_directory(_: &Path) -> Result<()> {
    Ok(())
}

fn dirs_fallback(app_name: &str) -> PathBuf {
    crate::platform::home_dir().join(".config").join(app_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_data_paths_must_be_absolute_and_disjoint() {
        let valid = Config {
            binaries_path: PathBuf::from("/tmp/bitengine-root/Binaries"),
            bitcoin_data_path: PathBuf::from("/tmp/bitengine-root/BitcoinChain"),
            electrs_data_path: PathBuf::from("/tmp/bitengine-root/ElectrsDB"),
        };
        valid
            .validate_paths()
            .expect("default-shaped paths are valid");

        let mut invalid = valid.clone();
        invalid.binaries_path = PathBuf::from("relative/Binaries");
        assert!(invalid.validate_paths().is_err());

        let mut overlapping = valid;
        overlapping.bitcoin_data_path = overlapping.binaries_path.join("chain");
        assert!(overlapping.validate_paths().is_err());
    }

    #[test]
    fn binaries_path_must_not_alias_the_derived_workspace() {
        let config = Config {
            binaries_path: PathBuf::from("/tmp/bitengine-root/BitEngineBuilds"),
            bitcoin_data_path: PathBuf::from("/tmp/bitengine-root/BitcoinChain"),
            electrs_data_path: PathBuf::from("/tmp/bitengine-root/ElectrsDB"),
        };
        assert!(config.validate_paths().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_aliases_are_detected() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let real = temporary.path().join("real");
        std::fs::create_dir(&real)?;
        let alias = temporary.path().join("alias");
        symlink(&real, &alias)?;
        let config = Config {
            binaries_path: alias.join("Binaries"),
            bitcoin_data_path: real.join("Binaries").join("chain"),
            electrs_data_path: real.join("ElectrsDB"),
        };
        assert!(config.validate_paths().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn atomic_config_write_replaces_symlink_without_touching_target() -> anyhow::Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("unrelated");
        let config_path = temporary.path().join("config.json");
        std::fs::write(&target, b"sentinel")?;
        symlink(&target, &config_path)?;

        write_atomically(&config_path, b"new config")?;

        assert_eq!(std::fs::read(&target)?, b"sentinel");
        assert_eq!(std::fs::read(&config_path)?, b"new config");
        let metadata = std::fs::symlink_metadata(&config_path)?;
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn config_directory_is_private_and_must_not_be_a_symlink() -> anyhow::Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let temporary = tempfile::tempdir()?;
        let private = temporary.path().join("private");
        prepare_config_directory(&private)?;
        assert_eq!(
            std::fs::metadata(&private)?.permissions().mode() & 0o777,
            0o700
        );

        let alias = temporary.path().join("alias");
        symlink(&private, &alias)?;
        assert!(prepare_config_directory(&alias).is_err());
        Ok(())
    }

    #[test]
    fn config_load_rejects_oversized_and_non_regular_files() -> anyhow::Result<()> {
        let temporary = tempfile::tempdir()?;
        let oversized = temporary.path().join("oversized.json");
        std::fs::write(
            &oversized,
            vec![b' '; usize::try_from(MAX_CONFIG_BYTES)? + 1],
        )?;
        assert!(Config::load_from_file(&oversized).is_err());

        let directory = temporary.path().join("directory.json");
        std::fs::create_dir(&directory)?;
        assert!(Config::load_from_file(&directory).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn config_load_does_not_follow_a_symbolic_link() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let target = temporary.path().join("target.json");
        let alias = temporary.path().join("config.json");
        std::fs::write(&target, b"{}")?;
        symlink(&target, &alias)?;

        assert!(Config::load_from_file(&alias).is_err());
        assert_eq!(std::fs::read(&target)?, b"{}");
        Ok(())
    }
}
