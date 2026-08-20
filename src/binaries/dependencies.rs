//! Cross-platform dependency detection and narrowly scoped installation.
//!
//! Scans resolve and execute the same sanitized tools used by source builds.
//! Install actions always perform a new full scan first and a second full scan
//! after every attempted change.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context as _, Result};
use tokio::process::Command;

use super::{
    environment::{self, find_in_path, BuildEnvironment},
    process, BinaryKind, ReleaseVersion,
};

pub const MINIMUM_RUST: [u64; 3] = [1, 91, 0];
const MINIMUM_CLANG: [u64; 3] = [17, 0, 0];
const MINIMUM_GCC: [u64; 3] = [12, 1, 0];
const MINIMUM_CMAKE: [u64; 3] = [3, 22, 0];
const MINIMUM_BOOST: [u64; 3] = [1, 74, 0];
const MINIMUM_LIBEVENT: [u64; 3] = [2, 1, 8];
const MINIMUM_OPENSSH: [u64; 3] = [8, 1, 0];
const INSTALL_TIMEOUT: Duration = Duration::from_mins(30);
const MAX_OS_RELEASE_BYTES: u64 = 64 * 1024;
const MAX_RUSTUP_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;
const RUSTUP_INSTALLER_URL: &str = "https://sh.rustup.rs";
static INSTALL_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyId {
    XcodeTools,
    Git,
    GnuPg,
    SshKeygen,
    CMake,
    Compiler,
    BuildTool,
    PkgConfig,
    Python,
    Boost,
    Libevent,
    RustToolchain,
    Libclang,
}

impl DependencyId {
    const fn label(self) -> &'static str {
        match self {
            Self::XcodeTools => "Xcode Command Line Tools",
            Self::Git => "Git",
            Self::GnuPg => "GnuPG",
            Self::SshKeygen => "OpenSSH signing tools",
            Self::CMake => "CMake",
            Self::Compiler => "C/C++ compiler",
            Self::BuildTool => "Build tool",
            Self::PkgConfig => "pkg-config",
            Self::Python => "Python",
            Self::Boost => "Boost",
            Self::Libevent => "libevent",
            Self::RustToolchain => "Rust toolchain",
            Self::Libclang => "libclang",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyState {
    Ready,
    Missing,
    Outdated,
}

impl DependencyState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Missing => "Missing",
            Self::Outdated => "Outdated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyItem {
    pub id: DependencyId,
    pub name: &'static str,
    pub detected_version: Option<String>,
    pub path: Option<PathBuf>,
    pub state: DependencyState,
    pub detail: Option<String>,
}

impl DependencyItem {
    fn missing(id: DependencyId, detail: impl Into<String>) -> Self {
        Self {
            id,
            name: id.label(),
            detected_version: None,
            path: None,
            state: DependencyState::Missing,
            detail: Some(detail.into()),
        }
    }

    const fn ready(
        id: DependencyId,
        version: Option<String>,
        path: Option<PathBuf>,
        detail: Option<String>,
    ) -> Self {
        Self {
            id,
            name: id.label(),
            detected_version: version,
            path,
            state: DependencyState::Ready,
            detail,
        }
    }

    fn outdated(
        id: DependencyId,
        version: Option<String>,
        path: Option<PathBuf>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id,
            name: id.label(),
            detected_version: version,
            path,
            state: DependencyState::Outdated,
            detail: Some(detail.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyPlatform {
    MacOs,
    Debian,
    Ubuntu,
    UnsupportedLinux(String),
    Unsupported(String),
}

impl DependencyPlatform {
    #[must_use]
    pub const fn supports_installation(&self) -> bool {
        matches!(self, Self::MacOs | Self::Debian | Self::Ubuntu)
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::MacOs => "macOS",
            Self::Debian => "Debian",
            Self::Ubuntu => "Ubuntu",
            Self::UnsupportedLinux(name) | Self::Unsupported(name) => name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DependencyReport {
    pub platform: DependencyPlatform,
    pub items: Vec<DependencyItem>,
    pub guidance: String,
}

impl DependencyReport {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.items
            .iter()
            .all(|item| item.state == DependencyState::Ready)
    }

    #[must_use]
    pub fn issue_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.state != DependencyState::Ready)
            .count()
    }

    #[must_use]
    pub fn issue_summary(&self) -> String {
        self.items
            .iter()
            .filter(|item| item.state != DependencyState::Ready)
            .map(|item| {
                item.detected_version.as_deref().map_or_else(
                    || format!("{} {}", item.name, item.state.label()),
                    |version| format!("{} {} (found {version})", item.name, item.state.label()),
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone)]
pub struct DependencyInstallOutcome {
    pub report: DependencyReport,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct NumericVersion([u64; 3]);

impl NumericVersion {
    const fn is_at_least(self, minimum: [u64; 3]) -> bool {
        let [major, minor, patch] = self.0;
        let [minimum_major, minimum_minor, minimum_patch] = minimum;
        major > minimum_major
            || (major == minimum_major
                && (minor > minimum_minor || (minor == minimum_minor && patch >= minimum_patch)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompilerFamily {
    Clang,
    Gcc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RustManager {
    Rustup,
    Homebrew,
    Apt,
    Manual,
    Missing,
}

pub async fn scan_all() -> DependencyReport {
    let base_environment = environment::build_environment();
    let environment = environment::bitcoin_environment(&base_environment);
    let platform = current_platform();
    let ids = all_dependency_ids(&platform);
    scan_selected(platform, &environment, &ids).await
}

pub async fn check(
    kind: BinaryKind,
    version: &ReleaseVersion,
    environment: &BuildEnvironment,
) -> DependencyReport {
    let platform = current_platform();
    let ids = build_dependency_ids(&platform, kind, version);
    let bitcoin_environment;
    let environment = if kind == BinaryKind::BitcoinCore {
        bitcoin_environment = environment::bitcoin_environment(environment);
        &bitcoin_environment
    } else {
        environment
    };
    scan_selected(platform, environment, &ids).await
}

pub async fn install_required() -> DependencyInstallOutcome {
    install_required_with(scan_all, attempt_install).await
}

async fn install_required_with<Scan, ScanFuture, Install, InstallFuture>(
    mut scan: Scan,
    install: Install,
) -> DependencyInstallOutcome
where
    Scan: FnMut() -> ScanFuture,
    ScanFuture: std::future::Future<Output = DependencyReport>,
    Install: FnOnce(DependencyReport) -> InstallFuture,
    InstallFuture: std::future::Future<Output = Result<String>>,
{
    let before = scan().await;
    if before.is_ready() {
        return DependencyInstallOutcome {
            report: before,
            message: "All build dependencies are already ready; nothing was installed.".to_owned(),
        };
    }

    let message = install(before)
        .await
        .unwrap_or_else(|error| format!("Dependency installation did not complete: {error:#}"));

    // A complete fresh scan is authoritative even after a command fails.
    let report = scan().await;
    DependencyInstallOutcome { report, message }
}

async fn attempt_install(before: DependencyReport) -> Result<String> {
    let environment = environment::build_environment();
    let attempt = match &before.platform {
        DependencyPlatform::MacOs => install_macos(&before, &environment).await,
        DependencyPlatform::Debian | DependencyPlatform::Ubuntu => {
            install_apt(&before, &environment).await
        }
        DependencyPlatform::UnsupportedLinux(name) => Ok((
            false,
            format!(
                "{name} is not supported for automatic dependency installation. No package-manager command was run."
            ),
        )),
        DependencyPlatform::Unsupported(name) => Ok((
            false,
            format!(
                "Automatic build-dependency installation is not supported on {name}. No system command was run."
            ),
        )),
    };
    attempt.map(|(_changed_system, message)| message)
}

fn all_dependency_ids(platform: &DependencyPlatform) -> Vec<DependencyId> {
    let mut ids = vec![
        DependencyId::Git,
        DependencyId::GnuPg,
        DependencyId::SshKeygen,
        DependencyId::CMake,
        DependencyId::Compiler,
        DependencyId::BuildTool,
        DependencyId::PkgConfig,
        DependencyId::Boost,
        DependencyId::Libevent,
        DependencyId::RustToolchain,
        DependencyId::Libclang,
    ];
    match platform {
        DependencyPlatform::MacOs => ids.insert(0, DependencyId::XcodeTools),
        DependencyPlatform::Debian
        | DependencyPlatform::Ubuntu
        | DependencyPlatform::UnsupportedLinux(_) => ids.push(DependencyId::Python),
        DependencyPlatform::Unsupported(_) => {}
    }
    ids
}

fn build_dependency_ids(
    platform: &DependencyPlatform,
    kind: BinaryKind,
    version: &ReleaseVersion,
) -> Vec<DependencyId> {
    let mut ids = vec![
        DependencyId::Git,
        DependencyId::CMake,
        DependencyId::Compiler,
        DependencyId::BuildTool,
    ];
    if matches!(platform, DependencyPlatform::MacOs) {
        ids.insert(0, DependencyId::XcodeTools);
    }
    match kind {
        BinaryKind::BitcoinCore => {
            ids.extend([
                DependencyId::GnuPg,
                DependencyId::PkgConfig,
                DependencyId::Boost,
                DependencyId::Libevent,
            ]);
            if matches!(
                platform,
                DependencyPlatform::Debian
                    | DependencyPlatform::Ubuntu
                    | DependencyPlatform::UnsupportedLinux(_)
            ) {
                ids.push(DependencyId::Python);
            }
        }
        BinaryKind::Electrs => {
            ids.push(if version.is_at_least([0, 11, 1]) {
                DependencyId::SshKeygen
            } else {
                DependencyId::GnuPg
            });
            ids.extend([DependencyId::RustToolchain, DependencyId::Libclang]);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

async fn scan_selected(
    platform: DependencyPlatform,
    environment: &BuildEnvironment,
    ids: &[DependencyId],
) -> DependencyReport {
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        items.push(scan_item(*id, environment).await);
    }
    let guidance = platform_guidance(&platform, &items, environment);
    DependencyReport {
        platform,
        items,
        guidance,
    }
}

async fn scan_item(id: DependencyId, environment: &BuildEnvironment) -> DependencyItem {
    match id {
        DependencyId::XcodeTools => scan_xcode_tools(environment).await,
        DependencyId::Git => {
            scan_simple_tool(id, &["git"], &["--version"], None, environment).await
        }
        DependencyId::GnuPg => {
            scan_simple_tool(id, &["gpg", "gpg2"], &["--version"], None, environment).await
        }
        DependencyId::SshKeygen => scan_openssh_signing(environment).await,
        DependencyId::CMake => {
            scan_simple_tool(
                id,
                &["cmake"],
                &["--version"],
                Some(MINIMUM_CMAKE),
                environment,
            )
            .await
        }
        DependencyId::Compiler => scan_compiler(environment).await,
        DependencyId::BuildTool => {
            scan_simple_tool(
                id,
                &["ninja", "make", "gmake"],
                &["--version"],
                None,
                environment,
            )
            .await
        }
        DependencyId::PkgConfig => {
            scan_simple_tool(id, &["pkg-config"], &["--version"], None, environment).await
        }
        DependencyId::Python => {
            scan_simple_tool(id, &["python3"], &["--version"], None, environment).await
        }
        DependencyId::Boost => scan_boost(environment).await,
        DependencyId::Libevent => {
            scan_pkg_config_library(id, "libevent", MINIMUM_LIBEVENT, environment).await
        }
        DependencyId::RustToolchain => scan_rust(environment).await,
        DependencyId::Libclang => scan_libclang(environment).await,
    }
}

async fn scan_openssh_signing(environment: &BuildEnvironment) -> DependencyItem {
    let Some(keygen_path) = find_in_path("ssh-keygen", environment) else {
        return DependencyItem::missing(
            DependencyId::SshKeygen,
            "ssh-keygen was not found in BitEngine's build PATH",
        );
    };
    let Some(ssh_path) = find_in_path("ssh", environment) else {
        return DependencyItem::missing(
            DependencyId::SshKeygen,
            "ssh was not found with the required ssh-keygen signing tool",
        );
    };
    if keygen_path.parent() != ssh_path.parent() {
        return DependencyItem::missing(
            DependencyId::SshKeygen,
            "ssh and ssh-keygen did not resolve from the same OpenSSH tool directory",
        );
    }
    let Some(program) = ssh_path.to_str() else {
        return DependencyItem::missing(
            DependencyId::SshKeygen,
            "the resolved ssh executable path is not valid UTF-8",
        );
    };
    let output =
        if let Some(stderr) = process::probe_stderr(program, &["-V"], None, environment).await {
            stderr
        } else if let Some(stdout) = process::probe(program, &["-V"], None, environment).await {
            stdout
        } else {
            return DependencyItem::missing(
                DependencyId::SshKeygen,
                "the resolved OpenSSH tools did not run successfully",
            );
        };
    let Some((version, text)) = extract_numeric_version(&output) else {
        return DependencyItem::outdated(
            DependencyId::SshKeygen,
            None,
            Some(keygen_path),
            format!(
                "could not validate OpenSSH {} or newer for SSH signature verification",
                display_version(MINIMUM_OPENSSH)
            ),
        );
    };
    if !version.is_at_least(MINIMUM_OPENSSH) {
        return DependencyItem::outdated(
            DependencyId::SshKeygen,
            Some(text),
            Some(keygen_path),
            format!(
                "BitEngine requires OpenSSH {} or newer for SSH signature verification",
                display_version(MINIMUM_OPENSSH)
            ),
        );
    }
    DependencyItem::ready(
        DependencyId::SshKeygen,
        Some(text),
        Some(keygen_path),
        Some(format!("OpenSSH client: {}", ssh_path.display())),
    )
}

async fn scan_simple_tool(
    id: DependencyId,
    candidates: &[&str],
    arguments: &[&str],
    minimum: Option<[u64; 3]>,
    environment: &BuildEnvironment,
) -> DependencyItem {
    let Some(path) = candidates
        .iter()
        .find_map(|candidate| find_in_path(candidate, environment))
    else {
        return DependencyItem::missing(
            id,
            "the executable was not found in BitEngine's build PATH",
        );
    };
    let Some(program) = path.to_str() else {
        return DependencyItem::missing(id, "the resolved executable path is not valid UTF-8");
    };
    let Some(output) = process::probe(program, arguments, None, environment).await else {
        return DependencyItem::missing(id, "the resolved executable did not run successfully");
    };
    let version = extract_numeric_version(&output).map(|(_, text)| text);
    if let Some(minimum) = minimum {
        let Some((parsed, text)) = extract_numeric_version(&output) else {
            return DependencyItem::outdated(
                id,
                None,
                Some(path),
                format!(
                    "could not validate the minimum version {}",
                    display_version(minimum)
                ),
            );
        };
        if !parsed.is_at_least(minimum) {
            return DependencyItem::outdated(
                id,
                Some(text),
                Some(path),
                format!("BitEngine requires {} or newer", display_version(minimum)),
            );
        }
    }
    DependencyItem::ready(id, version, Some(path), None)
}

async fn scan_compiler(environment: &BuildEnvironment) -> DependencyItem {
    let mut best_outdated = None;
    for (compiler, cxx) in [("clang", "clang++"), ("gcc", "g++"), ("cc", "c++")] {
        let (Some(path), Some(cxx_path)) = (
            find_in_path(compiler, environment),
            find_in_path(cxx, environment),
        ) else {
            continue;
        };
        let Some(program) = path.to_str() else {
            continue;
        };
        let Some(output) = process::probe(program, &["--version"], None, environment).await else {
            continue;
        };
        let Some((version, text)) = extract_numeric_version(&output) else {
            continue;
        };
        let family = if output.to_ascii_lowercase().contains("clang") {
            CompilerFamily::Clang
        } else {
            CompilerFamily::Gcc
        };
        let minimum = match family {
            CompilerFamily::Clang => MINIMUM_CLANG,
            CompilerFamily::Gcc => MINIMUM_GCC,
        };
        let detail = format!("C++ compiler: {}", cxx_path.display());
        if version.is_at_least(minimum) {
            return DependencyItem::ready(
                DependencyId::Compiler,
                Some(text),
                Some(path),
                Some(detail),
            );
        }
        best_outdated = Some(DependencyItem::outdated(
            DependencyId::Compiler,
            Some(text),
            Some(path),
            format!(
                "BitEngine requires Clang {}+ or GCC {}+; {detail}",
                display_version(MINIMUM_CLANG),
                display_version(MINIMUM_GCC)
            ),
        ));
    }
    best_outdated.unwrap_or_else(|| {
        DependencyItem::missing(
            DependencyId::Compiler,
            "no usable C and C++ compiler pair was found",
        )
    })
}

async fn scan_boost(environment: &BuildEnvironment) -> DependencyItem {
    for header in [
        "/opt/homebrew/include/boost/version.hpp",
        "/opt/homebrew/opt/boost/include/boost/version.hpp",
        "/usr/local/include/boost/version.hpp",
        "/usr/local/opt/boost/include/boost/version.hpp",
        "/usr/include/boost/version.hpp",
    ] {
        let path = Path::new(header);
        let Some(macro_value) = read_boost_macro(path) else {
            continue;
        };
        return boost_item(macro_value, Some(path.to_path_buf()));
    }

    let Some(cxx) = ["clang++", "g++", "c++"]
        .into_iter()
        .find_map(|name| find_in_path(name, environment))
    else {
        return DependencyItem::missing(
            DependencyId::Boost,
            "Boost headers could not be validated without a C++ compiler",
        );
    };
    let Some(program) = cxx.to_str() else {
        return DependencyItem::missing(DependencyId::Boost, "the C++ compiler path is invalid");
    };
    let output = process::probe(
        program,
        &[
            "-E",
            "-dM",
            "-include",
            "boost/version.hpp",
            "-x",
            "c++",
            "/dev/null",
        ],
        None,
        environment,
    )
    .await;
    let Some(macro_value) = output.as_deref().and_then(parse_boost_macro) else {
        return DependencyItem::missing(
            DependencyId::Boost,
            "the active C++ compiler could not include boost/version.hpp",
        );
    };
    boost_item(macro_value, None)
}

fn boost_item(macro_value: u64, path: Option<PathBuf>) -> DependencyItem {
    let version = NumericVersion([
        macro_value / 100_000,
        (macro_value / 100) % 1_000,
        macro_value % 100,
    ]);
    let text = display_version(version.0);
    if version.is_at_least(MINIMUM_BOOST) {
        DependencyItem::ready(DependencyId::Boost, Some(text), path, None)
    } else {
        DependencyItem::outdated(
            DependencyId::Boost,
            Some(text),
            path,
            format!(
                "BitEngine requires Boost {} or newer",
                display_version(MINIMUM_BOOST)
            ),
        )
    }
}

fn read_boost_macro(path: &Path) -> Option<u64> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    let contents = fs::read_to_string(path).ok()?;
    parse_boost_macro(&contents)
}

async fn scan_pkg_config_library(
    id: DependencyId,
    package: &str,
    minimum: [u64; 3],
    environment: &BuildEnvironment,
) -> DependencyItem {
    let Some(path) = find_in_path("pkg-config", environment) else {
        return DependencyItem::missing(id, "pkg-config is unavailable for library validation");
    };
    let Some(program) = path.to_str() else {
        return DependencyItem::missing(id, "the pkg-config path is invalid");
    };
    let Some(output) = process::probe(program, &["--modversion", package], None, environment).await
    else {
        return DependencyItem::missing(
            id,
            format!("pkg-config could not resolve {package} development files"),
        );
    };
    let Some((version, text)) = extract_numeric_version(&output) else {
        return DependencyItem::outdated(id, None, None, "the detected library version is invalid");
    };
    if version.is_at_least(minimum) {
        DependencyItem::ready(id, Some(text), None, None)
    } else {
        DependencyItem::outdated(
            id,
            Some(text),
            None,
            format!("BitEngine requires {} or newer", display_version(minimum)),
        )
    }
}

async fn scan_rust(environment: &BuildEnvironment) -> DependencyItem {
    let rustc = find_in_path("rustc", environment);
    let cargo = find_in_path("cargo", environment);
    let (Some(rustc), Some(cargo)) = (rustc.as_ref(), cargo.as_ref()) else {
        let detail = if rustc.is_none() && cargo.is_none() {
            "rustc and cargo were not found"
        } else if rustc.is_none() {
            "rustc was not found"
        } else {
            "cargo was not found"
        };
        return DependencyItem::missing(DependencyId::RustToolchain, detail);
    };
    let (Some(rustc_program), Some(cargo_program)) = (rustc.to_str(), cargo.to_str()) else {
        return DependencyItem::missing(
            DependencyId::RustToolchain,
            "a resolved Rust tool path is not valid UTF-8",
        );
    };
    let rustc_output = process::probe(rustc_program, &["--version"], None, environment).await;
    let cargo_output = process::probe(cargo_program, &["--version"], None, environment).await;
    let (Some(rustc_output), Some(cargo_output)) = (rustc_output, cargo_output) else {
        return DependencyItem::missing(
            DependencyId::RustToolchain,
            "rustc and cargo must both execute successfully",
        );
    };
    let Some((version, text)) = rustc_output
        .strip_prefix("rustc ")
        .and_then(extract_numeric_version)
    else {
        return DependencyItem::outdated(
            DependencyId::RustToolchain,
            None,
            Some(rustc.clone()),
            "rustc reported an unrecognized version",
        );
    };
    let cargo_version = cargo_output
        .strip_prefix("cargo ")
        .and_then(extract_numeric_version)
        .map_or_else(|| "unknown".to_owned(), |(_, text)| text);
    let rustup = rustup_diagnostic(environment).await;
    let detail = rustup.map_or_else(
        || format!("cargo {cargo_version} at {}", cargo.display()),
        |active| {
            format!(
                "cargo {cargo_version} at {}; rustup: {active}",
                cargo.display()
            )
        },
    );
    if version.is_at_least(MINIMUM_RUST) {
        DependencyItem::ready(
            DependencyId::RustToolchain,
            Some(text),
            Some(rustc.clone()),
            Some(detail),
        )
    } else {
        DependencyItem::outdated(
            DependencyId::RustToolchain,
            Some(text),
            Some(rustc.clone()),
            format!(
                "BitEngine requires Rust {} or newer; {detail}",
                display_version(MINIMUM_RUST)
            ),
        )
    }
}

async fn rustup_diagnostic(environment: &BuildEnvironment) -> Option<String> {
    let rustup = find_in_path("rustup", environment)?;
    process::probe(
        rustup.to_str()?,
        &["show", "active-toolchain"],
        None,
        environment,
    )
    .await
}

async fn scan_libclang(environment: &BuildEnvironment) -> DependencyItem {
    let path = resolve_libclang(environment).await;
    path.map_or_else(
        || {
            DependencyItem::missing(
                DependencyId::Libclang,
                "no usable libclang shared library was found",
            )
        },
        |path| DependencyItem::ready(DependencyId::Libclang, None, Some(path), None),
    )
}

async fn resolve_libclang(environment: &BuildEnvironment) -> Option<PathBuf> {
    if let Some(path) = environment.get("LIBCLANG_PATH") {
        if let Some(library) = libclang_in(Path::new(path)) {
            return Some(library);
        }
    }

    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/opt/llvm/lib"),
        PathBuf::from("/usr/local/opt/llvm/lib"),
        PathBuf::from("/Library/Developer/CommandLineTools/usr/lib"),
        PathBuf::from(
            "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib",
        ),
        PathBuf::from("/usr/lib/llvm/lib"),
        PathBuf::from("/usr/lib64"),
        PathBuf::from("/usr/lib"),
    ];
    for version in (15..=22).rev() {
        candidates.push(PathBuf::from(format!("/usr/lib/llvm-{version}/lib")));
    }
    if let Some(xcode_select) = find_in_path("xcode-select", environment) {
        if let Some(developer) =
            process::probe(xcode_select.to_str()?, &["-p"], None, environment).await
        {
            candidates
                .push(Path::new(&developer).join("Toolchains/XcodeDefault.xctoolchain/usr/lib"));
            candidates.push(Path::new(&developer).join("usr/lib"));
        }
    }
    if let Some(llvm_config) = find_in_path("llvm-config", environment) {
        if let Some(libdir) =
            process::probe(llvm_config.to_str()?, &["--libdir"], None, environment).await
        {
            candidates.push(PathBuf::from(libdir));
        }
    }
    candidates.into_iter().find_map(|path| libclang_in(&path))
}

fn libclang_in(path: &Path) -> Option<PathBuf> {
    if path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("libclang."))
    {
        return Some(path.to_path_buf());
    }
    let entries = fs::read_dir(path).ok()?;
    entries
        .filter_map(std::result::Result::ok)
        .find_map(|entry| {
            let candidate = entry.path();
            candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == "libclang.dylib"
                        || name == "libclang.so"
                        || name.starts_with("libclang.so.")
                })
                .then_some(candidate)
        })
}

async fn scan_xcode_tools(environment: &BuildEnvironment) -> DependencyItem {
    let (Some(xcode_select), Some(xcrun)) = (
        find_in_path("xcode-select", environment),
        find_in_path("xcrun", environment),
    ) else {
        return DependencyItem::missing(
            DependencyId::XcodeTools,
            "install Apple's Xcode Command Line Tools before using Homebrew dependencies",
        );
    };
    let developer = process::probe(
        xcode_select.to_str().unwrap_or_default(),
        &["-p"],
        None,
        environment,
    )
    .await;
    let clang = process::probe(
        xcrun.to_str().unwrap_or_default(),
        &["--find", "clang"],
        None,
        environment,
    )
    .await;
    match (developer, clang) {
        (Some(developer), Some(clang)) => DependencyItem::ready(
            DependencyId::XcodeTools,
            None,
            Some(PathBuf::from(clang)),
            Some(format!("developer directory: {developer}")),
        ),
        _ => DependencyItem::missing(
            DependencyId::XcodeTools,
            "Apple's developer tools are selected but clang is not usable",
        ),
    }
}

fn platform_guidance(
    platform: &DependencyPlatform,
    items: &[DependencyItem],
    environment: &BuildEnvironment,
) -> String {
    if items
        .iter()
        .all(|item| item.state == DependencyState::Ready)
    {
        return "All required tools and libraries validated successfully.".to_owned();
    }
    match platform {
        DependencyPlatform::MacOs if find_in_path("brew", environment).is_none() => {
            "Homebrew is required for missing third-party build dependencies. Install it from https://brew.sh, then run Check Dependencies again. BitEngine will not guess a Homebrew location.".to_owned()
        }
        DependencyPlatform::MacOs => {
            "Install only the dependencies marked Missing or Outdated. Xcode tools use Apple's guided installer; third-party packages use the resolved Homebrew executable.".to_owned()
        }
        DependencyPlatform::Debian | DependencyPlatform::Ubuntu => {
            "Only required packages are selected. BitEngine uses apt without a system-wide upgrade and delegates privilege elevation to the operating system.".to_owned()
        }
        DependencyPlatform::UnsupportedLinux(name) => format!(
            "{name} is not supported for automatic installation. BitEngine will not run a guessed package-manager command."
        ),
        DependencyPlatform::Unsupported(name) => {
            format!("Automatic dependency installation is unavailable on {name}.")
        }
    }
}

fn current_platform() -> DependencyPlatform {
    match std::env::consts::OS {
        "macos" => DependencyPlatform::MacOs,
        "linux" => detect_linux_platform(Path::new("/etc/os-release")),
        other => DependencyPlatform::Unsupported(other.to_owned()),
    }
}

fn detect_linux_platform(path: &Path) -> DependencyPlatform {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.len() <= MAX_OS_RELEASE_BYTES =>
        {
            metadata
        }
        _ => return DependencyPlatform::UnsupportedLinux("unsupported Linux".to_owned()),
    };
    if metadata.len() > MAX_OS_RELEASE_BYTES {
        return DependencyPlatform::UnsupportedLinux("unsupported Linux".to_owned());
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return DependencyPlatform::UnsupportedLinux("unsupported Linux".to_owned());
    };
    platform_from_os_release(&contents)
}

fn platform_from_os_release(contents: &str) -> DependencyPlatform {
    let id = contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key == "ID").then(|| value.trim_matches('"').to_ascii_lowercase())
    });
    match id.as_deref() {
        Some("debian") => DependencyPlatform::Debian,
        Some("ubuntu") => DependencyPlatform::Ubuntu,
        Some(other) => DependencyPlatform::UnsupportedLinux(format!("Linux ({other})")),
        None => DependencyPlatform::UnsupportedLinux("unsupported Linux".to_owned()),
    }
}

async fn install_macos(
    report: &DependencyReport,
    environment: &BuildEnvironment,
) -> Result<(bool, String)> {
    if report
        .items
        .iter()
        .any(|item| item.id == DependencyId::XcodeTools && item.state != DependencyState::Ready)
    {
        let xcode_select = find_in_path("xcode-select", environment)
            .context("xcode-select is unavailable; install Apple's Command Line Tools manually")?;
        run_checked(&xcode_select, &["--install"], environment).await?;
        return Ok((
            true,
            "Apple's Command Line Tools installer was opened. Complete it, then check dependencies again.".to_owned(),
        ));
    }

    let brew = find_in_path("brew", environment).context(
        "Homebrew is missing. Install it from https://brew.sh, then check dependencies again",
    )?;
    let mut install = BTreeSet::new();
    let mut upgrade = BTreeSet::new();
    let mut blockers = Vec::new();
    let mut changed = false;

    for item in report
        .items
        .iter()
        .filter(|item| item.state != DependencyState::Ready)
    {
        if item.id == DependencyId::RustToolchain {
            match rust_manager(item, environment).await {
                RustManager::Rustup => {
                    let rustup = find_in_path("rustup", environment)
                        .context("rustup was detected but its executable disappeared")?;
                    run_checked(&rustup, &["update", "stable"], environment).await?;
                    changed = true;
                }
                RustManager::Homebrew | RustManager::Missing => {
                    select_brew_formula(item, "rust", &brew, environment, &mut install, &mut upgrade, &mut blockers).await;
                }
                RustManager::Apt | RustManager::Manual => blockers.push(
                    "The outdated Rust toolchain is not managed by Homebrew or rustup; upgrade that existing installation to Rust 1.91+ to avoid a duplicate toolchain.".to_owned(),
                ),
            }
            continue;
        }
        if let Some(formula) = macos_formula(item.id) {
            select_brew_formula(
                item,
                formula,
                &brew,
                environment,
                &mut install,
                &mut upgrade,
                &mut blockers,
            )
            .await;
        } else if item.id == DependencyId::Compiler {
            blockers.push(
                "Update Apple's Command Line Tools so the selected compiler is Clang 17 or newer."
                    .to_owned(),
            );
        }
    }

    if !install.is_empty() {
        let mut arguments = vec!["install".to_owned()];
        arguments.extend(install);
        run_checked_owned(&brew, &arguments, environment).await?;
        changed = true;
    }
    if !upgrade.is_empty() {
        let mut arguments = vec!["upgrade".to_owned()];
        arguments.extend(upgrade);
        run_checked_owned(&brew, &arguments, environment).await?;
        changed = true;
    }
    let message = if blockers.is_empty() {
        "Required Homebrew dependencies were installed or upgraded, then checked again.".to_owned()
    } else if changed {
        format!(
            "Supported dependency changes completed. Manual attention is still required: {}",
            blockers.join(" ")
        )
    } else {
        blockers.join(" ")
    };
    Ok((changed, message))
}

async fn select_brew_formula(
    item: &DependencyItem,
    formula: &str,
    brew: &Path,
    environment: &BuildEnvironment,
    install: &mut BTreeSet<String>,
    upgrade: &mut BTreeSet<String>,
    blockers: &mut Vec<String>,
) {
    let listed = process::probe(
        brew.to_str().unwrap_or_default(),
        &["list", "--versions", formula],
        None,
        environment,
    )
    .await
    .is_some();
    match (item.state, listed) {
        (DependencyState::Missing, false) => {
            install.insert(formula.to_owned());
        }
        (DependencyState::Outdated, true) => {
            upgrade.insert(formula.to_owned());
        }
        (DependencyState::Missing, true) => blockers.push(format!(
            "Homebrew already owns {formula}, but BitEngine could not validate it; fix its PATH/linkage instead of reinstalling it."
        )),
        (DependencyState::Outdated, false) => blockers.push(format!(
            "{} is outdated but is not owned by Homebrew; upgrade its existing installation instead of installing a duplicate {formula} formula.",
            item.name
        )),
        (DependencyState::Ready, _) => {}
    }
}

async fn install_apt(
    report: &DependencyReport,
    environment: &BuildEnvironment,
) -> Result<(bool, String)> {
    let apt_get = find_in_path("apt-get", environment)
        .context("apt-get is unavailable; no guessed package-manager command will be executed")?;
    let mut packages = BTreeSet::new();
    let mut blockers = Vec::new();
    let mut changed = false;

    for item in report
        .items
        .iter()
        .filter(|item| item.state != DependencyState::Ready)
    {
        if item.id == DependencyId::RustToolchain {
            match rust_manager(item, environment).await {
                RustManager::Rustup => {
                    let rustup = find_in_path("rustup", environment)
                        .context("rustup was detected but its executable disappeared")?;
                    run_checked(&rustup, &["update", "stable"], environment).await?;
                    changed = true;
                }
                RustManager::Apt => {
                    if apt_rust_candidate(environment)
                        .await
                        .is_some_and(|version| version.is_at_least(MINIMUM_RUST))
                    {
                        packages.extend(["cargo".to_owned(), "rustc".to_owned()]);
                    } else {
                        blockers.push(
                            "The installed distro Rust is outdated and the configured apt candidate is below 1.91. Upgrade that distro-managed toolchain rather than adding a conflicting second installation."
                                .to_owned(),
                        );
                    }
                }
                RustManager::Missing => {
                    if apt_rust_candidate(environment)
                        .await
                        .is_some_and(|version| version.is_at_least(MINIMUM_RUST))
                    {
                        packages.extend(["cargo".to_owned(), "rustc".to_owned()]);
                    } else {
                        install_rustup_stable(environment).await?;
                        changed = true;
                    }
                }
                RustManager::Homebrew | RustManager::Manual => blockers.push(
                    "The outdated Rust toolchain is not managed by apt or rustup; upgrade that existing installation to Rust 1.91+ to avoid a duplicate toolchain.".to_owned(),
                ),
            }
            continue;
        }
        if let Some(package) = apt_package(item.id) {
            let installed = apt_package_installed(package, environment).await;
            match (item.state, installed) {
                (DependencyState::Missing, false) | (DependencyState::Outdated, true) => {
                    packages.insert(package.to_owned());
                }
                (DependencyState::Missing, true) => blockers.push(format!(
                    "apt already owns {package}, but BitEngine could not validate it; repair its PATH or development-file configuration instead of reinstalling it."
                )),
                (DependencyState::Outdated, false) => blockers.push(format!(
                    "{} is outdated but is not owned by apt; upgrade its existing installation instead of installing a duplicate {package} package.",
                    item.name
                )),
                (DependencyState::Ready, _) => {}
            }
        }
    }

    if !packages.is_empty() {
        run_apt(&apt_get, &["update".to_owned()], environment).await?;
        let mut arguments = vec![
            "install".to_owned(),
            "-y".to_owned(),
            "--no-install-recommends".to_owned(),
        ];
        arguments.extend(packages);
        run_apt(&apt_get, &arguments, environment).await?;
        changed = true;
    }
    let message = if blockers.is_empty() {
        "Required apt dependencies were installed or upgraded, then checked again.".to_owned()
    } else if changed {
        format!(
            "Supported dependency changes completed. Manual attention is still required: {}",
            blockers.join(" ")
        )
    } else {
        blockers.join(" ")
    };
    Ok((changed, message))
}

async fn run_apt(
    apt_get: &Path,
    arguments: &[String],
    environment: &BuildEnvironment,
) -> Result<()> {
    // SAFETY: `geteuid` has no arguments or memory-safety preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return run_checked_owned(apt_get, arguments, environment).await;
    }
    if let Some(pkexec) = find_in_path("pkexec", environment) {
        let mut elevated = vec![apt_get.display().to_string()];
        elevated.extend_from_slice(arguments);
        return run_checked_owned(&pkexec, &elevated, environment).await;
    }
    let sudo = find_in_path("sudo", environment).context(
        "neither pkexec nor sudo is available for normal operating-system privilege elevation",
    )?;
    let mut elevated = vec![apt_get.display().to_string()];
    elevated.extend_from_slice(arguments);
    run_checked_owned(&sudo, &elevated, environment).await
}

async fn apt_package_installed(package: &str, environment: &BuildEnvironment) -> bool {
    let Some(dpkg_query) = find_in_path("dpkg-query", environment) else {
        return false;
    };
    process::probe(
        dpkg_query.to_str().unwrap_or_default(),
        &["-W", "-f=${Status}", package],
        None,
        environment,
    )
    .await
    .is_some_and(|status| status.contains("install ok installed"))
}

async fn apt_rust_candidate(environment: &BuildEnvironment) -> Option<NumericVersion> {
    let apt_cache = find_in_path("apt-cache", environment)?;
    let output =
        process::probe(apt_cache.to_str()?, &["policy", "rustc"], None, environment).await?;
    parse_apt_rust_candidate(&output)
}

fn parse_apt_rust_candidate(output: &str) -> Option<NumericVersion> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Candidate:")
            .and_then(extract_numeric_version)
            .map(|(version, _)| version)
    })
}

async fn rust_manager(item: &DependencyItem, environment: &BuildEnvironment) -> RustManager {
    let Some(rustc) = item.path.as_deref() else {
        return if find_in_path("rustup", environment).is_some() {
            RustManager::Rustup
        } else {
            RustManager::Missing
        };
    };
    if let Some(rustup) = find_in_path("rustup", environment) {
        if let Some(rustup_rustc) = process::probe(
            rustup.to_str().unwrap_or_default(),
            &["which", "rustc"],
            None,
            environment,
        )
        .await
        {
            let same = fs::canonicalize(rustc)
                .ok()
                .zip(fs::canonicalize(rustup_rustc).ok())
                .is_some_and(|(actual, managed)| actual == managed);
            if same {
                return RustManager::Rustup;
            }
        }
    }
    let text = rustc.to_string_lossy();
    if text.contains("/homebrew/") || text.contains("/Cellar/rust/") {
        RustManager::Homebrew
    } else if text.starts_with("/usr/bin/") {
        RustManager::Apt
    } else {
        RustManager::Manual
    }
}

async fn install_rustup_stable(environment: &BuildEnvironment) -> Result<()> {
    if let Some(rustup) = find_in_path("rustup", environment) {
        run_checked(
            &rustup,
            &["toolchain", "install", "stable", "--profile", "minimal"],
            environment,
        )
        .await?;
        return run_checked(&rustup, &["default", "stable"], environment).await;
    }

    let curl = find_in_path("curl", environment).context(
        "the apt Rust candidate is below 1.91 and curl is unavailable; install current stable Rust from https://rustup.rs, then check dependencies again",
    )?;
    let shell = find_in_path("sh", environment).context("a POSIX shell is unavailable")?;
    let temporary = create_install_temporary_directory()?;
    let script = temporary.join("rustup-init.sh");
    let download_arguments = vec![
        "--proto".to_owned(),
        "=https".to_owned(),
        "--tlsv1.2".to_owned(),
        "--fail".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        RUSTUP_INSTALLER_URL.to_owned(),
        "--output".to_owned(),
        script.display().to_string(),
    ];
    let result = async {
        run_checked_owned(&curl, &download_arguments, environment).await?;
        validate_rustup_script(&script)?;
        let arguments = vec![
            script.display().to_string(),
            "-y".to_owned(),
            "--profile".to_owned(),
            "minimal".to_owned(),
            "--default-toolchain".to_owned(),
            "stable".to_owned(),
        ];
        run_checked_owned(&shell, &arguments, environment).await
    }
    .await;
    let _ = fs::remove_file(&script);
    let _ = fs::remove_dir(&temporary);
    result
}

fn create_install_temporary_directory() -> Result<PathBuf> {
    let root = std::env::temp_dir();
    for _ in 0..64 {
        let id = INSTALL_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let path = root.join(format!(
            "bitengine-rustup-{}-{timestamp}-{id}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create installer directory {}", path.display()));
            }
        }
    }
    bail!("could not allocate a private Rust installer directory")
}

fn validate_rustup_script(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect downloaded Rust installer {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_RUSTUP_SCRIPT_BYTES
    {
        bail!("downloaded Rust installer is not a bounded regular file");
    }
    let _ = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open downloaded Rust installer {}", path.display()))?;
    Ok(())
}

async fn run_checked(
    program: &Path,
    arguments: &[&str],
    environment: &BuildEnvironment,
) -> Result<()> {
    let arguments = arguments
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect::<Vec<_>>();
    run_checked_owned(program, &arguments, environment).await
}

async fn run_checked_owned(
    program: &Path,
    arguments: &[String],
    environment: &BuildEnvironment,
) -> Result<()> {
    let resolved = fs::canonicalize(program)
        .with_context(|| format!("resolve dependency installer {}", program.display()))?;
    let metadata = fs::metadata(&resolved)
        .with_context(|| format!("inspect dependency installer {}", resolved.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "dependency installer is not a regular executable: {}",
            resolved.display()
        );
    }
    let mut command = Command::new(&resolved);
    command
        .args(arguments)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(INSTALL_TIMEOUT, command.status())
        .await
        .with_context(|| format!("{} timed out", resolved.display()))?
        .with_context(|| format!("start {}", resolved.display()))?;
    if !status.success() {
        bail!(
            "{} exited with {} while installing required dependencies",
            resolved.display(),
            status
        );
    }
    Ok(())
}

const fn macos_formula(id: DependencyId) -> Option<&'static str> {
    match id {
        DependencyId::Git => Some("git"),
        DependencyId::GnuPg => Some("gnupg"),
        DependencyId::CMake => Some("cmake"),
        DependencyId::PkgConfig => Some("pkgconf"),
        DependencyId::Boost => Some("boost"),
        DependencyId::Libevent => Some("libevent"),
        DependencyId::Libclang => Some("llvm"),
        DependencyId::RustToolchain => Some("rust"),
        DependencyId::XcodeTools
        | DependencyId::SshKeygen
        | DependencyId::Compiler
        | DependencyId::BuildTool
        | DependencyId::Python => None,
    }
}

const fn apt_package(id: DependencyId) -> Option<&'static str> {
    match id {
        DependencyId::Git => Some("git"),
        DependencyId::GnuPg => Some("gnupg"),
        DependencyId::SshKeygen => Some("openssh-client"),
        DependencyId::CMake => Some("cmake"),
        DependencyId::Compiler | DependencyId::BuildTool => Some("build-essential"),
        DependencyId::PkgConfig => Some("pkgconf"),
        DependencyId::Python => Some("python3"),
        DependencyId::Boost => Some("libboost-dev"),
        DependencyId::Libevent => Some("libevent-dev"),
        DependencyId::Libclang => Some("libclang-dev"),
        DependencyId::RustToolchain => Some("rustc"),
        DependencyId::XcodeTools => None,
    }
}

fn parse_boost_macro(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        line.strip_prefix("#define BOOST_VERSION ")?
            .trim()
            .parse()
            .ok()
    })
}

fn extract_numeric_version(output: &str) -> Option<(NumericVersion, String)> {
    output.split_whitespace().find_map(|token| {
        let clean = token.trim_matches(|character: char| !character.is_ascii_digit());
        let prefix = clean
            .split(|character: char| !character.is_ascii_digit() && character != '.')
            .next()?;
        let mut components = prefix.split('.');
        let major = components.next()?.parse::<u64>().ok()?;
        let minor = components.next()?.parse::<u64>().ok()?;
        let patch = components
            .next()
            .filter(|component| !component.is_empty())
            .map_or(Some(0), |component| component.parse::<u64>().ok())?;
        Some((
            NumericVersion([major, minor, patch]),
            display_version([major, minor, patch]),
        ))
    })
}

fn display_version([major, minor, patch]: [u64; 3]) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependency_report(state: DependencyState) -> DependencyReport {
        let item = match state {
            DependencyState::Ready => DependencyItem::ready(
                DependencyId::Git,
                Some("2.51.0".to_owned()),
                Some(PathBuf::from("/usr/bin/git")),
                None,
            ),
            DependencyState::Missing => {
                DependencyItem::missing(DependencyId::Git, "fixture is missing")
            }
            DependencyState::Outdated => DependencyItem::outdated(
                DependencyId::Git,
                Some("1.0.0".to_owned()),
                Some(PathBuf::from("/usr/bin/git")),
                "fixture is outdated",
            ),
        };
        DependencyReport {
            platform: DependencyPlatform::Ubuntu,
            items: vec![item],
            guidance: String::new(),
        }
    }

    #[test]
    fn linux_platform_detection_is_fail_closed() {
        assert_eq!(
            platform_from_os_release("NAME=Debian\nID=debian\n"),
            DependencyPlatform::Debian
        );
        assert_eq!(
            platform_from_os_release("NAME=Ubuntu\nID=\"ubuntu\"\n"),
            DependencyPlatform::Ubuntu
        );
        assert!(matches!(
            platform_from_os_release("NAME=Fedora\nID=fedora\n"),
            DependencyPlatform::UnsupportedLinux(_)
        ));
        assert!(matches!(
            platform_from_os_release("NAME=Unknown\n"),
            DependencyPlatform::UnsupportedLinux(_)
        ));
    }

    #[test]
    fn version_parser_accepts_newer_and_rejects_unstructured_output() {
        let (minimum, _) = extract_numeric_version("rustc 1.91.0 (hash date)")
            .expect("minimum version should parse");
        let (newer, _) =
            extract_numeric_version("rustc 1.104.3").expect("newer version should parse");
        assert!(minimum.is_at_least(MINIMUM_RUST));
        assert!(newer.is_at_least(MINIMUM_RUST));
        assert!(extract_numeric_version("rustc nightly").is_none());
        assert!(parse_apt_rust_candidate("  Candidate: 1.91.0+dfsg")
            .is_some_and(|version| version.is_at_least(MINIMUM_RUST)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn openssh_signing_detection_uses_the_suite_version_instead_of_a_krl_query() -> Result<()>
    {
        use std::os::unix::fs::PermissionsExt as _;

        for (version, expected) in [
            ("8.0p1", DependencyState::Outdated),
            ("8.1p1", DependencyState::Ready),
            ("10.3p1", DependencyState::Ready),
        ] {
            let temporary = tempfile::tempdir()?;
            let keygen = temporary.path().join("ssh-keygen");
            fs::write(
                &keygen,
                "#!/bin/sh\necho 'the dependency scan must not query a KRL' >&2\nexit 99\n",
            )?;
            fs::set_permissions(&keygen, fs::Permissions::from_mode(0o755))?;

            let ssh = temporary.path().join("ssh");
            fs::write(
                &ssh,
                format!(
                    "#!/bin/sh\n[ \"$1\" = '-V' ] || exit 98\necho 'OpenSSH_{version}, fixture' >&2\n"
                ),
            )?;
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755))?;

            let mut environment = BuildEnvironment::new();
            environment.insert("PATH".to_owned(), temporary.path().display().to_string());

            let item = scan_item(DependencyId::SshKeygen, &environment).await;

            assert_eq!(
                item.state, expected,
                "unexpected state for OpenSSH {version}"
            );
            assert_eq!(item.path.as_deref(), Some(keygen.as_path()));
        }
        Ok(())
    }

    #[test]
    fn package_selection_excludes_disabled_bitcoin_features_and_system_rocksdb() {
        let bitcoin_packages = [
            DependencyId::Compiler,
            DependencyId::CMake,
            DependencyId::PkgConfig,
            DependencyId::Python,
            DependencyId::Boost,
            DependencyId::Libevent,
        ]
        .into_iter()
        .filter_map(apt_package)
        .collect::<BTreeSet<_>>();
        assert!(bitcoin_packages.contains("build-essential"));
        assert!(bitcoin_packages.contains("libevent-dev"));
        assert!(!bitcoin_packages.iter().any(|package| {
            package.contains("qt")
                || package.contains("zmq")
                || package.contains("sqlite")
                || package.contains("capnp")
        }));

        let electrs_packages = [
            DependencyId::Compiler,
            DependencyId::CMake,
            DependencyId::RustToolchain,
            DependencyId::Libclang,
        ]
        .into_iter()
        .filter_map(apt_package)
        .collect::<BTreeSet<_>>();
        assert!(electrs_packages.contains("libclang-dev"));
        assert!(!electrs_packages
            .iter()
            .any(|package| package.contains("rocksdb")));
    }

    #[test]
    fn debian_and_ubuntu_select_only_actual_build_packages() {
        for platform in [DependencyPlatform::Debian, DependencyPlatform::Ubuntu] {
            let packages = all_dependency_ids(&platform)
                .into_iter()
                .filter_map(apt_package)
                .collect::<BTreeSet<_>>();
            for required in [
                "build-essential",
                "cmake",
                "git",
                "gnupg",
                "libboost-dev",
                "libclang-dev",
                "libevent-dev",
                "openssh-client",
                "pkgconf",
                "python3",
            ] {
                assert!(packages.contains(required), "missing {required}");
            }
            assert!(!packages.iter().any(|package| {
                package.contains("capnp")
                    || package.contains("qt")
                    || package.contains("rocksdb")
                    || package.contains("sqlite")
                    || package.contains("zmq")
            }));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn path_visible_rust_is_detected_by_execution_and_msrv() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        for (version, expected) in [
            ("1.90.9", DependencyState::Outdated),
            ("1.91.0", DependencyState::Ready),
            ("1.104.1", DependencyState::Ready),
        ] {
            let temporary = tempfile::tempdir()?;
            for (tool, output) in [
                ("rustc", format!("rustc {version} (fixture)")),
                ("cargo", format!("cargo {version} (fixture)")),
            ] {
                let path = temporary.path().join(tool);
                fs::write(&path, format!("#!/bin/sh\necho '{output}'\n"))?;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
            let mut environment = BuildEnvironment::new();
            environment.insert("PATH".to_owned(), temporary.path().display().to_string());

            let item = scan_rust(&environment).await;

            assert_eq!(item.state, expected, "unexpected state for Rust {version}");
            assert_eq!(
                item.path.as_deref(),
                Some(temporary.path().join("rustc").as_path())
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rust_manager_detection_covers_rustup_brew_apt_and_manual_paths() -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        let rustc = temporary.path().join("rustc");
        let rustup = temporary.path().join("rustup");
        fs::write(&rustc, "#!/bin/sh\nexit 0\n")?;
        fs::write(&rustup, format!("#!/bin/sh\necho '{}'\n", rustc.display()))?;
        fs::set_permissions(&rustc, fs::Permissions::from_mode(0o755))?;
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755))?;
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), temporary.path().display().to_string());
        let rustup_item = DependencyItem::outdated(
            DependencyId::RustToolchain,
            Some("1.90.0".to_owned()),
            Some(rustc),
            "fixture",
        );
        assert_eq!(
            rust_manager(&rustup_item, &environment).await,
            RustManager::Rustup
        );

        environment.insert("PATH".to_owned(), String::new());
        for (path, expected) in [
            ("/opt/homebrew/bin/rustc", RustManager::Homebrew),
            ("/usr/bin/rustc", RustManager::Apt),
            ("/opt/manual-rust/bin/rustc", RustManager::Manual),
        ] {
            let item = DependencyItem::outdated(
                DependencyId::RustToolchain,
                Some("1.90.0".to_owned()),
                Some(PathBuf::from(path)),
                "fixture",
            );
            assert_eq!(rust_manager(&item, &environment).await, expected);
        }
        Ok(())
    }

    #[tokio::test]
    async fn missing_rust_is_reported_missing() {
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), String::new());
        let item = scan_rust(&environment).await;
        assert_eq!(item.state, DependencyState::Missing);
    }

    #[test]
    fn macos_package_selection_is_narrow_and_uses_llvm_only_for_libclang() {
        assert_eq!(macos_formula(DependencyId::CMake), Some("cmake"));
        assert_eq!(macos_formula(DependencyId::Boost), Some("boost"));
        assert_eq!(macos_formula(DependencyId::Libevent), Some("libevent"));
        assert_eq!(macos_formula(DependencyId::Libclang), Some("llvm"));
        assert!(macos_formula(DependencyId::XcodeTools).is_none());
        let formulas = [
            DependencyId::CMake,
            DependencyId::Boost,
            DependencyId::Libevent,
            DependencyId::Libclang,
        ]
        .into_iter()
        .filter_map(macos_formula)
        .collect::<Vec<_>>();
        assert!(!formulas.iter().any(|formula| {
            formula.contains("qt")
                || formula.contains("zmq")
                || formula.contains("rocksdb")
                || formula.contains("capnp")
        }));
    }

    #[test]
    fn unsupported_platforms_never_claim_installation_support() {
        assert!(!DependencyPlatform::UnsupportedLinux("Fedora".to_owned()).supports_installation());
        assert!(!DependencyPlatform::Unsupported("Windows".to_owned()).supports_installation());
    }

    #[tokio::test]
    async fn fully_satisfied_scan_never_invokes_an_installer() {
        let scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let installations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scan_counter = std::sync::Arc::clone(&scans);
        let install_counter = std::sync::Arc::clone(&installations);
        let report = dependency_report(DependencyState::Ready);

        let outcome = install_required_with(
            move || {
                scan_counter.fetch_add(1, Ordering::Relaxed);
                let report = report.clone();
                async move { report }
            },
            move |_| {
                install_counter.fetch_add(1, Ordering::Relaxed);
                async { Ok("installer should not run".to_owned()) }
            },
        )
        .await;

        assert!(outcome.report.is_ready());
        assert_eq!(outcome.report.issue_count(), 0);
        assert_eq!(scans.load(Ordering::Relaxed), 1);
        assert_eq!(installations.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn installation_attempt_is_always_followed_by_a_fresh_scan() {
        let scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let installations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scan_counter = std::sync::Arc::clone(&scans);
        let install_counter = std::sync::Arc::clone(&installations);
        let before = dependency_report(DependencyState::Missing);
        let after = dependency_report(DependencyState::Ready);

        let outcome = install_required_with(
            move || {
                let report = if scan_counter.fetch_add(1, Ordering::Relaxed) == 0 {
                    before.clone()
                } else {
                    after.clone()
                };
                async move { report }
            },
            move |report| {
                assert!(!report.is_ready());
                install_counter.fetch_add(1, Ordering::Relaxed);
                async { Ok("fixture installation completed".to_owned()) }
            },
        )
        .await;

        assert!(outcome.report.is_ready());
        assert_eq!(scans.load(Ordering::Relaxed), 2);
        assert_eq!(installations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn failed_dependency_installation_does_not_touch_binary_transaction_state() -> Result<()>
    {
        let temporary = tempfile::tempdir()?;
        let installed_binary = temporary.path().join("bitcoind");
        let transaction_journal = temporary.path().join(".bitengine-install-journal.json");
        fs::write(&installed_binary, b"existing binary")?;
        fs::write(&transaction_journal, b"existing transaction state")?;
        let scans = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let scan_counter = std::sync::Arc::clone(&scans);
        let report = dependency_report(DependencyState::Outdated);

        let outcome = install_required_with(
            move || {
                scan_counter.fetch_add(1, Ordering::Relaxed);
                let report = report.clone();
                async move { report }
            },
            |_| async { anyhow::bail!("fixture package manager failed") },
        )
        .await;

        assert!(!outcome.report.is_ready());
        assert!(outcome.message.contains("fixture package manager failed"));
        assert_eq!(scans.load(Ordering::Relaxed), 2);
        assert_eq!(fs::read(installed_binary)?, b"existing binary");
        assert_eq!(
            fs::read(transaction_journal)?,
            b"existing transaction state"
        );
        Ok(())
    }
}
