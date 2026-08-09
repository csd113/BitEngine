//! Native Bitcoin Core and electrs build/update support.
//!
//! This module owns the reusable functionality that previously lived in the
//! standalone `BitForge` application: release discovery, installed-version
//! detection, dependency checks, source builds, progress/log reporting, and
//! safe installation into `BitEngine`'s configured binaries directory.

mod dependencies;
mod environment;
mod install;
mod process;
mod service;

use std::{
    cmp::Ordering,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::LazyLock,
    time::Duration,
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub use service::{
    BuildFailure, BuildRequest, BuildService, BuildSummary, PersistedBuild, PersistedBuildStatus,
};

const BITCOIN_RELEASES_API: &str =
    "https://api.github.com/repos/bitcoin/bitcoin/releases?per_page=30";
const ELECTRS_RELEASES_API: &str =
    "https://api.github.com/repos/romanz/electrs/releases?per_page=30";
const MAX_RELEASES: usize = 10;

static HTTP_CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|error| error.to_string())
});

/// A buildable binary family managed by `BitEngine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryKind {
    BitcoinCore,
    Electrs,
}

impl BinaryKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BitcoinCore => "Bitcoin Core",
            Self::Electrs => "electrs",
        }
    }

    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::BitcoinCore => "bitcoin",
            Self::Electrs => "electrs",
        }
    }

    pub(crate) const fn repository(self) -> &'static str {
        match self {
            Self::BitcoinCore => "https://github.com/bitcoin/bitcoin.git",
            Self::Electrs => "https://github.com/romanz/electrs.git",
        }
    }

    pub(crate) const fn primary_binary(self) -> &'static str {
        match self {
            Self::BitcoinCore => "bitcoind",
            Self::Electrs => "electrs",
        }
    }
}

/// A validated stable release tag and its comparable numeric version.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct ReleaseVersion {
    tag: String,
    parts: [u64; 3],
}

impl ReleaseVersion {
    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    #[must_use]
    pub fn display(&self) -> &str {
        self.tag.strip_prefix('v').unwrap_or(&self.tag)
    }
}

impl FromStr for ReleaseVersion {
    type Err = String;

    fn from_str(tag: &str) -> Result<Self, Self::Err> {
        let clean = tag.strip_prefix('v').unwrap_or(tag);
        let components = clean.split('.').collect::<Vec<_>>();
        if !(2..=3).contains(&components.len())
            || components.iter().any(|component| {
                component.is_empty() || !component.bytes().all(|b| b.is_ascii_digit())
            })
        {
            return Err(format!("unsupported release tag: {tag:?}"));
        }

        let mut parts = [0_u64; 3];
        for (index, component) in components.into_iter().enumerate() {
            parts[index] = component
                .parse::<u64>()
                .map_err(|_| format!("release number is too large: {tag:?}"))?;
        }

        Ok(Self {
            tag: tag.to_owned(),
            parts,
        })
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.display())
    }
}

impl PartialEq for ReleaseVersion {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl PartialOrd for ReleaseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReleaseVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts)
    }
}

/// Human-readable build stages shown by the binaries page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildStage {
    CheckingRequirements,
    DownloadingSource,
    VerifyingSource,
    PreparingBuild,
    Compiling,
    VerifyingBinary,
    Installing,
    Complete,
}

impl BuildStage {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CheckingRequirements => "Checking requirements",
            Self::DownloadingSource => "Downloading source",
            Self::VerifyingSource => "Verifying source",
            Self::PreparingBuild => "Preparing build",
            Self::Compiling => "Compiling",
            Self::VerifyingBinary => "Verifying binary",
            Self::Installing => "Installing",
            Self::Complete => "Complete",
        }
    }

    #[must_use]
    pub const fn progress(self) -> f32 {
        match self {
            Self::CheckingRequirements => 0.03,
            Self::DownloadingSource => 0.10,
            Self::VerifyingSource => 0.22,
            Self::PreparingBuild => 0.28,
            Self::Compiling => 0.38,
            Self::VerifyingBinary => 0.86,
            Self::Installing => 0.94,
            Self::Complete => 1.0,
        }
    }
}

/// Events streamed from a background build into `BitEngine`'s update loop.
#[derive(Debug, Clone)]
pub enum BuildEvent {
    Stage { kind: BinaryKind, stage: BuildStage },
    Progress(f32),
    Log(String),
}

/// Installed versions detected from `BitEngine`'s configured binaries directory.
#[derive(Debug, Clone)]
pub struct InstalledVersions {
    pub bitcoin: Result<Option<ReleaseVersion>, String>,
    pub electrs: Result<Option<ReleaseVersion>, String>,
}

impl InstalledVersions {
    /// Probe both installed binaries without waiting for the upstream service.
    pub async fn detect(binaries_dir: &Path) -> Self {
        let bitcoin_path = binaries_dir.join("bitcoind");
        let electrs_path = binaries_dir.join("electrs");
        let (bitcoin, electrs) = tokio::join!(
            probe_installed_version(&bitcoin_path),
            probe_installed_version(&electrs_path),
        );
        Self {
            bitcoin: bitcoin.map_err(|error| error.to_string()),
            electrs: electrs.map_err(|error| error.to_string()),
        }
    }
}

/// Stable releases currently available from both upstream projects.
#[derive(Debug, Clone)]
pub struct AvailableVersions {
    pub bitcoin: Result<Vec<ReleaseVersion>, String>,
    pub electrs: Result<Vec<ReleaseVersion>, String>,
}

impl AvailableVersions {
    /// Fetch both upstream release lists concurrently.
    pub async fn fetch() -> Self {
        let (bitcoin, electrs) = tokio::join!(
            fetch_releases(BinaryKind::BitcoinCore),
            fetch_releases(BinaryKind::Electrs),
        );
        Self {
            bitcoin: bitcoin.map_err(|error| error.to_string()),
            electrs: electrs.map_err(|error| error.to_string()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    prerelease: bool,
    draft: bool,
}

async fn fetch_releases(kind: BinaryKind) -> Result<Vec<ReleaseVersion>> {
    let url = match kind {
        BinaryKind::BitcoinCore => BITCOIN_RELEASES_API,
        BinaryKind::Electrs => ELECTRS_RELEASES_API,
    };
    let client = HTTP_CLIENT
        .as_ref()
        .map_err(|error| anyhow::anyhow!(error.clone()))?;
    let releases = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch {} releases", kind.label()))?
        .error_for_status()
        .with_context(|| format!("{} release service returned an error", kind.label()))?
        .json::<Vec<GitHubRelease>>()
        .await
        .with_context(|| format!("read {} release list", kind.label()))?;

    Ok(stable_versions(releases))
}

fn stable_versions(releases: Vec<GitHubRelease>) -> Vec<ReleaseVersion> {
    let mut versions: Vec<ReleaseVersion> = releases
        .into_iter()
        .filter(|release| {
            !release.prerelease
                && !release.draft
                && !release.tag_name.to_ascii_lowercase().contains("rc")
        })
        .filter_map(|release| release.tag_name.parse().ok())
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| right.cmp(left));
    versions.dedup();
    versions.truncate(MAX_RELEASES);
    versions
}

async fn probe_installed_version(path: &Path) -> Result<Option<ReleaseVersion>> {
    if !path.is_file() {
        return Ok(None);
    }

    let mut command = tokio::process::Command::new(path);
    command.arg("--version").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .with_context(|| format!("version check timed out for {}", path.display()))?
        .with_context(|| format!("run {} --version", path.display()))?;

    if !output.status.success() {
        anyhow::bail!("{} --version exited with {}", path.display(), output.status);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_version_output(&text)
        .map(Some)
        .with_context(|| format!("read version reported by {}", path.display()))
}

fn parse_version_output(output: &str) -> Result<ReleaseVersion> {
    output
        .split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '.')
            })
        })
        .find_map(|token| token.parse::<ReleaseVersion>().ok())
        .ok_or_else(|| anyhow::anyhow!("no stable version number found in output"))
}

/// Derive `BitEngine`'s private build workspace from its authoritative binaries
/// path. This avoids a second, drifting output-path configuration.
#[must_use]
pub fn workspace_for(binaries_dir: &Path) -> PathBuf {
    binaries_dir
        .parent()
        .unwrap_or(binaries_dir)
        .join("BitEngineBuilds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn installed_version_detection_runs_the_configured_binary() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        let binary = temporary.path().join("bitcoind");
        std::fs::write(&binary, "#!/bin/sh\necho 'Bitcoin Core version v30.0.1'\n")?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;

        let version = probe_installed_version(&binary)
            .await?
            .context("version should be present")?;
        assert_eq!(version.display(), "30.0.1");
        Ok(())
    }

    #[test]
    fn release_versions_validate_and_sort() {
        let older = "v29.2".parse::<ReleaseVersion>().expect("valid version");
        let newer = "30.0.1".parse::<ReleaseVersion>().expect("valid version");
        assert!(newer > older);
        assert_eq!(older.display(), "29.2");
        assert!("v30.0-rc1".parse::<ReleaseVersion>().is_err());
        assert!("30".parse::<ReleaseVersion>().is_err());
    }

    #[test]
    fn installed_version_output_is_detected() {
        let bitcoin = parse_version_output("Bitcoin Core version v30.0.0\n")
            .expect("bitcoin version should parse");
        let electrs =
            parse_version_output("electrs 0.10.10\n").expect("electrs version should parse");
        assert_eq!(bitcoin.display(), "30.0.0");
        assert_eq!(electrs.display(), "0.10.10");
    }

    #[test]
    fn stable_release_handling_filters_and_orders() {
        let releases = vec![
            GitHubRelease {
                tag_name: "v29.2".to_owned(),
                prerelease: false,
                draft: false,
            },
            GitHubRelease {
                tag_name: "v30.0rc1".to_owned(),
                prerelease: true,
                draft: false,
            },
            GitHubRelease {
                tag_name: "v30.0".to_owned(),
                prerelease: false,
                draft: false,
            },
        ];
        let versions = stable_versions(releases);
        assert_eq!(
            versions
                .iter()
                .map(ReleaseVersion::display)
                .collect::<Vec<_>>(),
            vec!["30.0", "29.2"]
        );
    }

    #[test]
    fn build_stage_progress_is_monotonic() {
        let stages = [
            BuildStage::CheckingRequirements,
            BuildStage::DownloadingSource,
            BuildStage::VerifyingSource,
            BuildStage::PreparingBuild,
            BuildStage::Compiling,
            BuildStage::VerifyingBinary,
            BuildStage::Installing,
            BuildStage::Complete,
        ];
        assert!(stages
            .windows(2)
            .all(|pair| pair[0].progress() < pair[1].progress()));
    }
}
