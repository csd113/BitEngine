//! Native build dependency checks.
//!
//! `BitEngine` reports missing tools with platform-specific guidance. It does
//! not silently mutate the host package manager while starting a build.

use std::path::Path;

use super::{
    environment::{find_in_path, BuildEnvironment},
    BinaryKind,
};

#[derive(Debug)]
pub struct DependencyReport {
    pub found: Vec<String>,
    pub missing: Vec<String>,
    pub guidance: String,
}

impl DependencyReport {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
    }
}

pub async fn check(kind: BinaryKind, environment: &BuildEnvironment) -> DependencyReport {
    let mut found = Vec::new();
    let mut missing = Vec::new();

    check_tool("Git", &["git"], environment, &mut found, &mut missing);

    match kind {
        BinaryKind::BitcoinCore => {
            check_tool("CMake", &["cmake"], environment, &mut found, &mut missing);
            check_tool(
                "C compiler",
                &["cc", "clang", "gcc"],
                environment,
                &mut found,
                &mut missing,
            );
            check_tool(
                "C++ compiler",
                &["c++", "clang++", "g++"],
                environment,
                &mut found,
                &mut missing,
            );
            check_tool(
                "Build tool",
                &["ninja", "make", "gmake"],
                environment,
                &mut found,
                &mut missing,
            );
            check_tool(
                "pkg-config",
                &["pkg-config"],
                environment,
                &mut found,
                &mut missing,
            );

            if find_in_path("pkg-config", environment).is_some() {
                let libevent_found = tokio::process::Command::new("pkg-config")
                    .args(["--exists", "libevent"])
                    .env_clear()
                    .envs(environment)
                    .output()
                    .await
                    .is_ok_and(|output| output.status.success());
                if libevent_found {
                    found.push("libevent".to_owned());
                } else {
                    missing.push("libevent development files".to_owned());
                }
            }
        }
        BinaryKind::Electrs => {
            check_tool("Cargo", &["cargo"], environment, &mut found, &mut missing);
            check_tool(
                "Rust compiler",
                &["rustc"],
                environment,
                &mut found,
                &mut missing,
            );
            check_tool("Clang", &["clang"], environment, &mut found, &mut missing);
            check_tool(
                "C++ compiler",
                &["c++", "clang++", "g++"],
                environment,
                &mut found,
                &mut missing,
            );
            check_tool("CMake", &["cmake"], environment, &mut found, &mut missing);
            check_tool(
                "Build tool",
                &["ninja", "make", "gmake"],
                environment,
                &mut found,
                &mut missing,
            );
            if has_libclang(environment) {
                found.push("libclang".to_owned());
            } else {
                missing.push("libclang development files".to_owned());
            }
        }
    }

    DependencyReport {
        found,
        missing,
        guidance: install_guidance(kind),
    }
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
            BinaryKind::BitcoinCore => "Install the missing tools with Homebrew: brew install cmake pkg-config libevent git llvm".to_owned(),
            BinaryKind::Electrs => "Install the missing tools with Homebrew: brew install rust cmake llvm git".to_owned(),
        };
    }

    match kind {
        BinaryKind::BitcoinCore => "Install your distribution's C/C++ build tools plus git, cmake, pkg-config, and libevent development files.".to_owned(),
        BinaryKind::Electrs => "Install Rust/Cargo, git, clang, libclang development files, and your distribution's native build tools.".to_owned(),
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
        assert!(electrs.contains("Rust") || electrs.contains("rust"));
    }
}
