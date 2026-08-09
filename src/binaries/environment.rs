//! Platform-aware environment construction for native source builds.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub type BuildEnvironment = HashMap<String, String>;

#[must_use]
pub fn build_environment() -> BuildEnvironment {
    let mut environment = std::env::vars().collect::<BuildEnvironment>();
    let mut paths = Vec::with_capacity(24);

    if cfg!(target_os = "macos") {
        push_path(&mut paths, "/opt/homebrew/bin");
    }

    if let Some(home) = environment.get("HOME") {
        let cargo_bin = Path::new(home).join(".cargo").join("bin");
        if cargo_bin.is_dir() {
            push_path(&mut paths, cargo_bin);
        }
    }

    let llvm_prefix = llvm_candidates()
        .into_iter()
        .find(|candidate| candidate.join("bin").is_dir());
    if let Some(prefix) = llvm_prefix.as_ref() {
        push_path(&mut paths, prefix.join("bin"));
    }

    if let Some(existing) = environment.get("PATH") {
        paths.extend(
            existing
                .split(':')
                .filter(|part| !part.is_empty())
                .map(ToOwned::to_owned),
        );
    }
    paths.extend(
        ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .into_iter()
            .map(ToOwned::to_owned),
    );

    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| !path.is_empty() && seen.insert(path.clone()));
    environment.insert("PATH".to_owned(), paths.join(":"));

    if let Some(prefix) = llvm_prefix {
        let library_path = prefix.join("lib").display().to_string();
        environment.insert("LIBCLANG_PATH".to_owned(), library_path.clone());
        if cfg!(target_os = "macos") {
            environment.insert("DYLD_LIBRARY_PATH".to_owned(), library_path);
        } else if cfg!(target_os = "linux") {
            environment.insert("LD_LIBRARY_PATH".to_owned(), library_path);
        }
    }

    environment
}

#[must_use]
pub fn bitcoin_environment(base: &BuildEnvironment) -> BuildEnvironment {
    let mut environment = base.clone();
    let mut pkg_config_paths = [
        "/opt/homebrew/lib/pkgconfig",
        "/opt/homebrew/share/pkgconfig",
        "/usr/local/lib/pkgconfig",
        "/usr/local/share/pkgconfig",
        "/usr/lib/pkgconfig",
        "/usr/share/pkgconfig",
        "/usr/lib64/pkgconfig",
        "/usr/lib/x86_64-linux-gnu/pkgconfig",
        "/usr/lib/aarch64-linux-gnu/pkgconfig",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect::<Vec<_>>();

    if let Some(existing) = environment.get("PKG_CONFIG_PATH") {
        for part in existing.split(':').filter(|part| !part.is_empty()) {
            if !pkg_config_paths.iter().any(|known| known == part) {
                pkg_config_paths.push(part.to_owned());
            }
        }
    }

    environment.insert("PKG_CONFIG_PATH".to_owned(), pkg_config_paths.join(":"));
    environment.remove("TERM");
    set_plain_output(&mut environment);
    environment.insert("GIT_PROGRESS_DELAY".to_owned(), "0".to_owned());
    environment
}

#[must_use]
pub fn cargo_environment(base: &BuildEnvironment, target_dir: &Path) -> BuildEnvironment {
    let mut environment = base.clone();
    set_plain_output(&mut environment);
    environment.insert("TERM".to_owned(), "dumb".to_owned());
    environment.insert("GIT_PROGRESS_DELAY".to_owned(), "0".to_owned());
    environment.insert("CARGO_TERM_COLOR".to_owned(), "never".to_owned());
    environment.insert("CARGO_TERM_PROGRESS_WHEN".to_owned(), "always".to_owned());
    environment.insert("CARGO_TERM_PROGRESS_WIDTH".to_owned(), "60".to_owned());
    environment.insert(
        "CARGO_TARGET_DIR".to_owned(),
        target_dir.display().to_string(),
    );
    environment
}

#[must_use]
pub fn find_in_path(tool: &str, environment: &BuildEnvironment) -> Option<PathBuf> {
    environment
        .get("PATH")?
        .split(':')
        .filter(|part| !part.is_empty())
        .map(|directory| Path::new(directory).join(tool))
        .find(|candidate| candidate.is_file())
}

fn set_plain_output(environment: &mut BuildEnvironment) {
    environment.insert("NO_COLOR".to_owned(), "1".to_owned());
    environment.insert("CLICOLOR".to_owned(), "0".to_owned());
    environment.insert("CLICOLOR_FORCE".to_owned(), "0".to_owned());
}

fn push_path(paths: &mut Vec<String>, path: impl AsRef<Path>) {
    paths.push(path.as_ref().display().to_string());
}

fn llvm_candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![PathBuf::from("/opt/homebrew/opt/llvm")]
    } else {
        [
            "/usr/lib/llvm",
            "/usr/lib/llvm-18",
            "/usr/lib/llvm-17",
            "/usr/lib/llvm-16",
            "/usr/lib/llvm-15",
            "/usr/lib64/llvm",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitcoin_environment_preserves_existing_pkg_config_paths() {
        let mut base = BuildEnvironment::new();
        base.insert("PKG_CONFIG_PATH".to_owned(), "/custom/pkgconfig".to_owned());
        let environment = bitcoin_environment(&base);
        let path = environment
            .get("PKG_CONFIG_PATH")
            .expect("pkg-config path should be set");
        assert!(path.contains("/custom/pkgconfig"));
        assert!(!environment.contains_key("TERM"));
    }
}
