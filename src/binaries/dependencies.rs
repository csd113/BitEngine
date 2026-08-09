//! Native build dependency checks.
//!
//! `BitEngine` reports missing tools with platform-specific guidance. It does
//! not silently mutate the host package manager while starting a build.

use std::path::Path;

use super::{
    environment::{find_in_path, BuildEnvironment},
    process, BinaryKind, ReleaseVersion,
};

const ELECTRS_0_11_1_MINIMUM_RUST: [u64; 3] = [1, 85, 0];

#[derive(Debug)]
pub struct DependencyReport {
    pub found: Vec<String>,
    pub missing: Vec<String>,
    pub guidance: String,
}

impl DependencyReport {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }
}

pub async fn check(
    kind: BinaryKind,
    version: &ReleaseVersion,
    environment: &BuildEnvironment,
) -> DependencyReport {
    let mut found = Vec::new();
    let mut missing = Vec::new();

    check_tool("Git", &["git"], environment, &mut found, &mut missing);

    match kind {
        BinaryKind::BitcoinCore => check_bitcoin(environment, &mut found, &mut missing).await,
        BinaryKind::Electrs => {
            check_electrs(version, environment, &mut found, &mut missing).await;
        }
    }

    DependencyReport {
        found,
        missing,
        guidance: install_guidance(kind),
    }
}

async fn check_bitcoin(
    environment: &BuildEnvironment,
    found: &mut Vec<String>,
    missing: &mut Vec<String>,
) {
    for (label, commands) in [
        ("GnuPG (release-tag verification)", &["gpg", "gpg2"][..]),
        ("CMake", &["cmake"][..]),
        ("C compiler", &["cc", "clang", "gcc"][..]),
        ("C++ compiler", &["c++", "clang++", "g++"][..]),
        ("Build tool", &["ninja", "make", "gmake"][..]),
        ("pkg-config", &["pkg-config"][..]),
    ] {
        check_tool(label, commands, environment, found, missing);
    }

    if find_in_path("pkg-config", environment).is_some() {
        if process::probe("pkg-config", &["--exists", "libevent"], None, environment)
            .await
            .is_some()
        {
            found.push("libevent".to_owned());
        } else {
            missing.push("libevent development files".to_owned());
        }
    }
    if has_boost_headers() {
        found.push("Boost headers".to_owned());
    } else {
        missing.push("Boost development headers".to_owned());
    }
}

async fn check_electrs(
    version: &ReleaseVersion,
    environment: &BuildEnvironment,
    found: &mut Vec<String>,
    missing: &mut Vec<String>,
) {
    let verifier = if version.is_at_least([0, 11, 1]) {
        ("ssh-keygen (release-tag verification)", &["ssh-keygen"][..])
    } else {
        ("GnuPG (release-tag verification)", &["gpg", "gpg2"][..])
    };
    check_tool(verifier.0, verifier.1, environment, found, missing);
    check_tool("Cargo", &["cargo"], environment, found, missing);
    check_rust_compiler(version, environment, found, missing).await;
    for (label, commands) in [
        ("Clang", &["clang"][..]),
        ("C++ compiler", &["c++", "clang++", "g++"][..]),
        ("CMake", &["cmake"][..]),
        ("Build tool", &["ninja", "make", "gmake"][..]),
    ] {
        check_tool(label, commands, environment, found, missing);
    }
    if has_libclang(environment) {
        found.push("libclang".to_owned());
    } else {
        missing.push("libclang development files".to_owned());
    }
}

async fn check_rust_compiler(
    target_version: &ReleaseVersion,
    environment: &BuildEnvironment,
    found: &mut Vec<String>,
    missing: &mut Vec<String>,
) {
    if !target_version.is_at_least([0, 11, 1]) {
        check_tool("Rust compiler", &["rustc"], environment, found, missing);
        return;
    }

    let compiler_version = process::probe("rustc", &["--version"], None, environment)
        .await
        .and_then(|output| parse_rustc_version(&output));
    match compiler_version {
        Some(version) if version.is_at_least(ELECTRS_0_11_1_MINIMUM_RUST) => found.push(format!(
            "Rust compiler {} (minimum 1.85 for electrs 0.11.1+)",
            version.display()
        )),
        Some(version) => missing.push(format!(
            "Rust compiler 1.85 or newer (found {})",
            version.display()
        )),
        None => missing.push("Rust compiler 1.85 or newer".to_owned()),
    }
}

fn parse_rustc_version(output: &str) -> Option<ReleaseVersion> {
    output
        .strip_prefix("rustc ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn has_boost_headers() -> bool {
    [
        "/opt/homebrew/include/boost/version.hpp",
        "/opt/homebrew/opt/boost/include/boost/version.hpp",
        "/usr/local/include/boost/version.hpp",
        "/usr/include/boost/version.hpp",
    ]
    .into_iter()
    .any(|path| Path::new(path).is_file())
}

fn check_tool(
    label: &str,
    commands: &[&str],
    environment: &BuildEnvironment,
    found: &mut Vec<String>,
    missing: &mut Vec<String>,
) {
    if let Some(command) = commands
        .iter()
        .find(|command| find_in_path(command, environment).is_some())
    {
        found.push(format!("{label} ({command})"));
    } else {
        missing.push(label.to_owned());
    }
}

fn has_libclang(environment: &BuildEnvironment) -> bool {
    if environment
        .get("LIBCLANG_PATH")
        .is_some_and(|path| Path::new(path).is_dir())
    {
        return true;
    }

    [
        "/opt/homebrew/opt/llvm/lib/libclang.dylib",
        "/usr/lib/llvm/lib/libclang.so",
        "/usr/lib/llvm-18/lib/libclang.so",
        "/usr/lib/llvm-17/lib/libclang.so",
        "/usr/lib/llvm-16/lib/libclang.so",
        "/usr/lib/llvm-15/lib/libclang.so",
        "/usr/lib64/libclang.so",
        "/usr/lib/libclang.so",
    ]
    .into_iter()
    .any(|path| Path::new(path).exists())
}

fn install_guidance(kind: BinaryKind) -> String {
    if cfg!(target_os = "macos") {
        return match kind {
            BinaryKind::BitcoinCore => "Install the missing tools with Homebrew: brew install cmake pkg-config libevent boost git gnupg llvm. Import the selected Bitcoin Core release-tag signing key and independently verify its full fingerprint through the official project guidance; BitEngine separately enforces its reviewed signer pins.".to_owned(),
            BinaryKind::Electrs => "Install the missing tools with Homebrew: brew install rust cmake llvm git gnupg. electrs v0.11.1 requires Rust 1.85 or newer and uses the pinned upstream SSH signer; older releases require the maintainer's imported OpenPGP key, whose full fingerprint BitEngine pins. OpenSSH's ssh-keygen is included with macOS.".to_owned(),
        };
    }

    match kind {
        BinaryKind::BitcoinCore => "Install your distribution's C/C++ build tools plus git, GnuPG, cmake, pkg-config, Boost, and libevent development files. Import the selected Bitcoin Core release-tag signing key and independently verify its full fingerprint through the official project guidance; BitEngine separately enforces its reviewed signer pins.".to_owned(),
        BinaryKind::Electrs => "Install Rust/Cargo, git, OpenSSH tools, GnuPG (for older releases), clang, libclang development files, and your distribution's native build tools. electrs v0.11.1 requires Rust 1.85 or newer and uses the pinned upstream SSH signer; older releases require the maintainer's imported OpenPGP key, whose full fingerprint BitEngine pins.".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guidance_is_specific_to_the_build_target() {
        let bitcoin = install_guidance(BinaryKind::BitcoinCore);
        let electrs = install_guidance(BinaryKind::Electrs);
        assert!(bitcoin.contains("libevent"));
        assert!(bitcoin.contains("full fingerprint"));
        assert!(electrs.contains("Rust") || electrs.contains("rust"));
    }

    #[test]
    fn current_electrs_uses_ssh_tag_verification() {
        let version = "v0.11.1"
            .parse::<ReleaseVersion>()
            .expect("valid release version");
        assert!(version.is_at_least([0, 11, 1]));
    }

    #[test]
    fn rustc_version_parser_is_strict_and_enforces_current_electrs_minimum() {
        let minimum = parse_rustc_version("rustc 1.85.0 (4d91de4e4 2025-02-17)")
            .expect("rustc output should parse");
        let too_old =
            parse_rustc_version("rustc 1.84.1 (hash date)").expect("rustc output should parse");
        assert!(minimum.is_at_least(ELECTRS_0_11_1_MINIMUM_RUST));
        assert!(!too_old.is_at_least(ELECTRS_0_11_1_MINIMUM_RUST));
        assert!(parse_rustc_version("cargo 1.85.0").is_none());
        assert!(parse_rustc_version("rustc nightly").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn current_electrs_rejects_rustc_older_than_minimum() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        for tool in [
            "git",
            "ssh-keygen",
            "cargo",
            "rustc",
            "clang",
            "c++",
            "cmake",
            "ninja",
        ] {
            let path = temporary.path().join(tool);
            let body = if tool == "rustc" {
                "#!/bin/sh\necho 'rustc 1.84.1 (test)'\n"
            } else {
                "#!/bin/sh\nexit 0\n"
            };
            std::fs::write(&path, body)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
        }
        let mut environment = BuildEnvironment::new();
        environment.insert("PATH".to_owned(), temporary.path().display().to_string());
        environment.insert(
            "LIBCLANG_PATH".to_owned(),
            temporary.path().display().to_string(),
        );
        let version = "v0.11.1"
            .parse::<ReleaseVersion>()
            .map_err(anyhow::Error::msg)?;

        let report = check(BinaryKind::Electrs, &version, &environment).await;

        assert!(report
            .missing
            .iter()
            .any(|dependency| dependency.contains("1.85") && dependency.contains("1.84.1")));
        Ok(())
    }
}
