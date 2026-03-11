//! Binary update system.
//!
//! Scans `~/Downloads/bitcoin_builds/binaries/` for versioned folders,
//! selects the highest semantic version, and copies the relevant binaries
//! into the configured `Binaries/` directory on the SSD.
//!
//! Folder naming convention expected:
//!   `bitcoin-27.0`          → contains bitcoind, bitcoin-cli, bitcoin-tx, bitcoin-util
//!   `electrs-0.10.5`        → contains electrs

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

// ── Version parsing ───────────────────────────────────────────────────────────

/// Parse a semantic version string like "27.0.1" into a comparable tuple.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = s.splitn(4, '.').collect();
    let major = parts.first()?.parse().ok()?;
    let minor = parts.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
    let patch = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// Find the folder with the highest version for a given `prefix` (e.g. "bitcoin")
/// inside `search_dir`.
///
/// Returns the folder name (e.g. "bitcoin-27.0") or `None`.
pub fn find_latest_version(search_dir: &Path, prefix: &str) -> Option<String> {
    let mut best: Option<((u64, u64, u64), String)> = None;

    let entries = fs::read_dir(search_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Must be a directory
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        // Must match `<prefix>-<version>`
        let Some(version_str) = name.strip_prefix(&format!("{prefix}-")) else {
            continue;
        };
        if let Some(ver) = parse_semver(version_str) {
            match &best {
                None => best = Some((ver, name)),
                Some((best_ver, _)) if ver > *best_ver => best = Some((ver, name)),
                _ => {}
            }
        }
    }

    best.map(|(_, name)| name)
}

// ── Copy helpers ──────────────────────────────────────────────────────────────

/// Copy a list of binary `names` from `src_dir` to `dst_dir`.
///
/// Each binary is first written to a `.tmp` file, then atomically renamed,
/// so a partial copy never replaces a working binary.
/// File permissions are set to 0o755 (rwxr-xr-x).
///
/// Returns the list of binary names that were actually copied.
pub fn copy_binaries(src_dir: &Path, dst_dir: &Path, names: &[&str]) -> Result<Vec<String>> {
    fs::create_dir_all(dst_dir)
        .with_context(|| format!("create binaries dir {}", dst_dir.display()))?;

    let mut copied = Vec::new();

    for &name in names {
        let src = src_dir.join(name);
        if !src.exists() {
            // Not every folder contains every binary — skip silently.
            continue;
        }

        let dst = dst_dir.join(name);
        let tmp = dst_dir.join(format!(".{name}.tmp"));

        // Write to temp first
        fs::copy(&src, &tmp).with_context(|| format!("copy {name} to temp {}", tmp.display()))?;

        // Set executable permissions before rename
        let mut perms = fs::metadata(&tmp)
            .with_context(|| format!("stat {}", tmp.display()))?
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms).with_context(|| format!("chmod {}", tmp.display()))?;

        // Atomic rename
        fs::rename(&tmp, &dst)
            .with_context(|| format!("rename {} → {}", tmp.display(), dst.display()))?;

        copied.push(name.to_owned());
    }

    Ok(copied)
}

// ── Update entry point ────────────────────────────────────────────────────────

/// Outcome of an update attempt.
#[derive(Debug)]
pub enum UpdateResult {
    /// At least one binary was updated.  Message lists what changed.
    Updated(String),
    /// `bitcoin_builds` not found but BitForge.app exists at the given path.
    BitForgeFound(PathBuf),
    /// `bitcoin_builds` not found and BitForge.app is absent.
    BitForgeNotFound,
    /// `bitcoin_builds` found but the `binaries/` sub-folder is missing.
    BinariesSubfolderMissing,
    /// `bitcoin_builds` and `binaries/` both found but no versioned folders inside.
    NothingToUpdate,
}

/// Run the full update check.
pub fn run_update(binaries_dst: &Path) -> UpdateResult {
    let downloads = home_dir().join("Downloads").join("bitcoin_builds");

    if !downloads.exists() {
        let bitforge = PathBuf::from("/Applications/BitForge.app");
        return if bitforge.exists() {
            UpdateResult::BitForgeFound(bitforge)
        } else {
            UpdateResult::BitForgeNotFound
        };
    }

    let binaries_src = downloads.join("binaries");
    if !binaries_src.exists() {
        return UpdateResult::BinariesSubfolderMissing;
    }

    let btc_folder = find_latest_version(&binaries_src, "bitcoin");
    let etr_folder = find_latest_version(&binaries_src, "electrs");

    if btc_folder.is_none() && etr_folder.is_none() {
        return UpdateResult::NothingToUpdate;
    }

    let mut messages: Vec<String> = Vec::new();

    if let Some(folder) = btc_folder {
        let src = binaries_src.join(&folder);
        match copy_binaries(
            &src,
            binaries_dst,
            &["bitcoind", "bitcoin-cli", "bitcoin-tx", "bitcoin-util"],
        ) {
            Ok(copied) if !copied.is_empty() => {
                messages.push(format!("Bitcoin ({folder}): {}", copied.join(", ")));
            }
            Ok(_) => {}
            Err(e) => messages.push(format!("Bitcoin update error: {e}")),
        }
    }

    if let Some(folder) = etr_folder {
        let src = binaries_src.join(&folder);
        match copy_binaries(&src, binaries_dst, &["electrs"]) {
            Ok(copied) if !copied.is_empty() => {
                messages.push(format!("Electrs ({folder}): {}", copied.join(", ")));
            }
            Ok(_) => {}
            Err(e) => messages.push(format!("Electrs update error: {e}")),
        }
    }

    if messages.is_empty() {
        UpdateResult::NothingToUpdate
    } else {
        UpdateResult::Updated(messages.join("\n"))
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_parsing() {
        assert_eq!(parse_semver("27.0"), Some((27, 0, 0)));
        assert_eq!(parse_semver("0.10.5"), Some((0, 10, 5)));
        assert_eq!(parse_semver("1"), Some((1, 0, 0)));
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn latest_version_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir(dir.join("bitcoin-26.0")).unwrap();
        std::fs::create_dir(dir.join("bitcoin-27.1")).unwrap();
        std::fs::create_dir(dir.join("bitcoin-27.0")).unwrap();
        let latest = find_latest_version(dir, "bitcoin");
        assert_eq!(latest.as_deref(), Some("bitcoin-27.1"));
    }
}
