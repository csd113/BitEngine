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

pub use dependencies::{
    install_required as install_build_dependencies, scan_all as scan_build_dependencies,
    DependencyInstallOutcome, DependencyReport, DependencyState,
};
pub use service::{
    BuildFailure, BuildRequest, BuildService, BuildSummary, PersistedBuild, PersistedBuildStatus,
};

const BITCOIN_RELEASES_API: &str =
    "https://api.github.com/repos/bitcoin/bitcoin/releases?per_page=30";
const ELECTRS_RELEASES_API: &str =
    "https://api.github.com/repos/romanz/electrs/releases?per_page=30";
const MAX_RELEASES: usize = 10;
const MAX_RELEASE_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_RELEASE_RESPONSE_BYTES_U64: u64 = 1024 * 1024;

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

/// UI-generated identity used to correlate every event and terminal result
/// belonging to one build request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildOperationId(pub u64);

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
}

/// A validated stable release tag and its comparable numeric version.
#[derive(Debug, Clone, Eq, Serialize)]
pub struct ReleaseVersion {
    tag: String,
    parts: [u64; 3],
}

impl<'de> Deserialize<'de> for ReleaseVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PersistedReleaseVersion {
            tag: String,
            #[serde(default)]
            parts: Option<[u64; 3]>,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum PersistedReleaseVersionRepresentation {
            Object(PersistedReleaseVersion),
            Tag(String),
        }

        let (tag, persisted_parts) =
            match PersistedReleaseVersionRepresentation::deserialize(deserializer)? {
                PersistedReleaseVersionRepresentation::Object(persisted) => {
                    (persisted.tag, persisted.parts)
                }
                PersistedReleaseVersionRepresentation::Tag(tag) => (tag, None),
            };
        let parsed = tag.parse::<Self>().map_err(serde::de::Error::custom)?;
        if let Some(parts) = persisted_parts {
            if parts != parsed.parts {
                return Err(serde::de::Error::custom(format!(
                    "release version parts do not match tag {tag:?}"
                )));
            }
        }
        Ok(parsed)
    }
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

    #[must_use]
    pub(crate) const fn is_at_least(&self, parts: [u64; 3]) -> bool {
        let [major, minor, patch] = self.parts;
        let [minimum_major, minimum_minor, minimum_patch] = parts;
        major > minimum_major
            || (major == minimum_major
                && (minor > minimum_minor || (minor == minimum_minor && patch >= minimum_patch)))
    }

    #[must_use]
    pub(crate) const fn is_exactly(&self, parts: [u64; 3]) -> bool {
        self.parts[0] == parts[0] && self.parts[1] == parts[1] && self.parts[2] == parts[2]
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
    Stage {
        operation_id: BuildOperationId,
        kind: BinaryKind,
        stage: BuildStage,
    },
    Progress {
        operation_id: BuildOperationId,
        progress: f32,
    },
    Log {
        operation_id: BuildOperationId,
        message: String,
    },
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
    let mut response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetch {} releases", kind.label()))?
        .error_for_status()
        .with_context(|| format!("{} release service returned an error", kind.label()))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_RESPONSE_BYTES_U64)
    {
        anyhow::bail!("{} release response is unexpectedly large", kind.label());
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(16 * 1024)
            .min(MAX_RELEASE_RESPONSE_BYTES),
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("read {} release response", kind.label()))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RELEASE_RESPONSE_BYTES {
            anyhow::bail!("{} release response is unexpectedly large", kind.label());
        }
        body.extend_from_slice(&chunk);
    }
    let releases = serde_json::from_slice::<Vec<GitHubRelease>>(&body)
        .with_context(|| format!("read {} release list", kind.label()))?;

    Ok(stable_versions(kind, releases))
}

fn stable_versions(kind: BinaryKind, releases: Vec<GitHubRelease>) -> Vec<ReleaseVersion> {
    let mut versions: Vec<ReleaseVersion> = releases
        .into_iter()
        .filter(|release| {
            !release.prerelease
                && !release.draft
                && !release.tag_name.to_ascii_lowercase().contains("rc")
        })
        .filter_map(|release| release.tag_name.parse().ok())
        .filter(|version: &ReleaseVersion| {
            (kind != BinaryKind::BitcoinCore || version.is_at_least([29, 0, 0]))
                && (kind != BinaryKind::Electrs
                    || !version.is_at_least([0, 11, 1])
                    || version.is_exactly([0, 11, 1]))
        })
        .collect::<Vec<_>>();
    versions.sort_by(|left, right| right.cmp(left));
    versions.dedup();
    versions.truncate(MAX_RELEASES);
    versions
}

async fn probe_installed_version(path: &Path) -> Result<Option<ReleaseVersion>> {
    let kind = match path.file_name().and_then(|name| name.to_str()) {
        Some("bitcoind") => BinaryKind::BitcoinCore,
        Some("electrs") => BinaryKind::Electrs,
        _ => anyhow::bail!(
            "cannot determine managed binary kind from path {}",
            path.display()
        ),
    };
    probe_binary_version(kind, path).await
}

async fn probe_binary_version(kind: BinaryKind, path: &Path) -> Result<Option<ReleaseVersion>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        anyhow::bail!("installed binary is not a regular file: {}", path.display());
    }

    let program = path.to_str().with_context(|| {
        format!(
            "installed binary path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    let environment = environment::build_environment();
    let output = process::probe(program, &["--version"], None, &environment)
        .await
        .with_context(|| {
            format!(
                "{} --version failed, timed out, or produced excessive output",
                path.display()
            )
        })?;

    parse_version_output(kind, &output)
        .map(Some)
        .with_context(|| format!("read version reported by {}", path.display()))
}

fn parse_version_output(kind: BinaryKind, output: &str) -> Result<ReleaseVersion> {
    let first_line = output
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .context("version output is empty")?;
    let version = match kind {
        BinaryKind::BitcoinCore => {
            let tokens = first_line
                .strip_prefix("Bitcoin Core ")
                .context("version output does not identify Bitcoin Core")?
                .split_whitespace()
                .collect::<Vec<_>>();
            tokens
                .windows(2)
                .find_map(|pair| (pair[0] == "version").then_some(pair[1]))
                .context("Bitcoin Core output has no version field")?
        }
        BinaryKind::Electrs => first_line
            .strip_prefix("electrs ")
            .unwrap_or(first_line)
            .split_whitespace()
            .next()
            .context("electrs output has no version field")?,
    };
    version
        .parse::<ReleaseVersion>()
        .map_err(anyhow::Error::msg)
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
    fn release_version_deserialization_recomputes_validated_parts() {
        let current_shape = r#"{"tag":"v30.0.1","parts":[30,0,1]}"#;
        let version = serde_json::from_str::<ReleaseVersion>(current_shape)
            .expect("current persisted shape should deserialize");
        assert_eq!(version.display(), "30.0.1");

        let compact = serde_json::from_str::<ReleaseVersion>(r#""v0.11.1""#)
            .expect("string representation should deserialize");
        assert_eq!(compact.display(), "0.11.1");

        let recomputed = serde_json::from_str::<ReleaseVersion>(r#"{"tag":"v29.2"}"#)
            .expect("parts omitted from an object should be recomputed");
        assert!(recomputed.is_at_least([29, 2, 0]));
        assert!(!recomputed.is_at_least([29, 2, 1]));
    }

    #[test]
    fn release_version_deserialization_rejects_inconsistent_or_malformed_state() {
        for invalid in [
            r#"{"tag":"v30.0.1","parts":[99,0,0]}"#,
            r#"{"tag":"v30.0-rc1","parts":[30,0,0]}"#,
            r#"{"tag":"v30.0.1","parts":[30,0,1],"extra":true}"#,
        ] {
            assert!(
                serde_json::from_str::<ReleaseVersion>(invalid).is_err(),
                "unexpectedly accepted {invalid}"
            );
        }
    }

    #[test]
    fn installed_version_output_is_detected() {
        let bitcoin = parse_version_output(
            BinaryKind::BitcoinCore,
            "Bitcoin Core daemon version v30.0.0 bitcoind\n",
        )
        .expect("bitcoin version should parse");
        let electrs = parse_version_output(BinaryKind::Electrs, "v0.10.10\n")
            .expect("electrs version should parse");
        assert_eq!(bitcoin.display(), "30.0.0");
        assert_eq!(electrs.display(), "0.10.10");
        assert!(parse_version_output(BinaryKind::Electrs, "wrapper v99.0\n").is_err());
        assert!(parse_version_output(BinaryKind::Electrs, "electrs 0.11.1-malformed\n").is_err());
        assert!(parse_version_output(
            BinaryKind::BitcoinCore,
            "Bitcoin Core RPC client version v31.1\n"
        )
        .is_ok());
        assert!(parse_version_output(
            BinaryKind::BitcoinCore,
            "electrs 30.0.0\nBitcoin Core version v30.0.0\n"
        )
        .is_err());
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
        let versions = stable_versions(BinaryKind::BitcoinCore, releases);
        assert_eq!(
            versions
                .iter()
                .map(ReleaseVersion::display)
                .collect::<Vec<_>>(),
            vec!["30.0", "29.2"]
        );
    }

    #[test]
    fn bitcoin_release_discovery_excludes_pre_cmake_versions() {
        let releases = vec![
            GitHubRelease {
                tag_name: "v29.0".to_owned(),
                prerelease: false,
                draft: false,
            },
            GitHubRelease {
                tag_name: "v28.2".to_owned(),
                prerelease: false,
                draft: false,
            },
        ];
        let versions = stable_versions(BinaryKind::BitcoinCore, releases);
        assert_eq!(
            versions
                .iter()
                .map(ReleaseVersion::display)
                .collect::<Vec<_>>(),
            vec!["29.0"]
        );
    }

    #[test]
    fn electrs_release_discovery_withholds_unreviewed_future_signers() {
        let releases = ["v0.11.0", "v0.11.1", "v0.11.2"]
            .into_iter()
            .map(|tag| GitHubRelease {
                tag_name: tag.to_owned(),
                prerelease: false,
                draft: false,
            })
            .collect();
        let versions = stable_versions(BinaryKind::Electrs, releases);
        assert_eq!(
            versions
                .iter()
                .map(ReleaseVersion::display)
                .collect::<Vec<_>>(),
            vec!["0.11.1", "0.11.0"]
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
