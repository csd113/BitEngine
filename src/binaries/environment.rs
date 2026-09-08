//! Platform-aware environment construction for native source builds.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

pub type BuildEnvironment = HashMap<String, String>;

const PASSTHROUGH_VARIABLES: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

#[must_use]
pub fn build_environment() -> BuildEnvironment {
    build_environment_from(|name| std::env::var(name).ok())
}

fn build_environment_from(mut variable: impl FnMut(&str) -> Option<String>) -> BuildEnvironment {
    // Build inputs such as GIT_CONFIG_*, RUSTFLAGS, RUSTC_WRAPPER,
    // CMAKE_TOOLCHAIN_FILE, and dynamic-loader injection variables must not be
    // inherited from BitEngine's launcher. The small allowlist below preserves
    // locale, temporary-directory, and proxy settings needed by normal builds.
    let mut environment = PASSTHROUGH_VARIABLES
        .iter()
        .filter_map(|name| {
            variable(name)
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_owned(), value))
        })
        .collect::<BuildEnvironment>();
    let mut paths = Vec::with_capacity(24);

    if cfg!(target_os = "macos") {
        push_existing_path(&mut paths, "/opt/homebrew/bin");
    }

    if let Some(home) = environment.get("HOME") {
        let cargo_bin = Path::new(home).join(".cargo").join("bin");
        push_existing_path(&mut paths, cargo_bin);
        let nix_profile_bin = Path::new(home).join(".nix-profile").join("bin");
        push_existing_path(&mut paths, nix_profile_bin);
    }

    let llvm_prefix = llvm_candidates()
        .into_iter()
        .find(|candidate| candidate.join("bin").is_dir());
    if let Some(prefix) = llvm_prefix.as_ref() {
        push_existing_path(&mut paths, prefix.join("bin"));
    }

    for path in [
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/nix/var/nix/profiles/default/bin",
        "/run/current-system/sw/bin",
        "/snap/bin",
    ] {
        push_existing_path(&mut paths, path);
    }

    // Honor additional PATH-visible manual installations only when the
    // directory itself is absolute, real, owned by root/current user, and not
    // writable by group or everyone. Known system paths stay ahead of these
    // entries, so launcher-provided PATH data cannot substitute build tools.
    if let Some(inherited_path) = variable("PATH") {
        for path in std::env::split_paths(&inherited_path) {
            push_safe_inherited_path(&mut paths, &path);
        }
    }

    let mut seen = HashSet::with_capacity(paths.len());
    paths.retain(|path| !path.is_empty() && seen.insert(path.clone()));
    environment.insert("PATH".to_owned(), paths.join(":"));
    environment.insert("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned());
    environment.insert("GIT_CONFIG_GLOBAL".to_owned(), "/dev/null".to_owned());
    environment.insert("GIT_CONFIG_COUNT".to_owned(), "0".to_owned());
    environment.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());

    if let Some(prefix) = llvm_prefix {
        // Locate libclang for bindgen without overriding the dynamic loader's
        // search path: rustc must load its own compatible LLVM libraries.
        environment.insert(
            "LIBCLANG_PATH".to_owned(),
            prefix.join("lib").display().to_string(),
        );
    }

    environment
}

#[must_use]
pub fn bitcoin_environment(base: &BuildEnvironment) -> BuildEnvironment {
    let mut environment = base.clone();
    let mut pkg_config_paths = [
        "/opt/homebrew/lib/pkgconfig",
        "/opt/homebrew/share/pkgconfig",
        "/opt/homebrew/opt/libevent/lib/pkgconfig",
        "/usr/local/lib/pkgconfig",
        "/usr/local/share/pkgconfig",
        "/usr/local/opt/libevent/lib/pkgconfig",
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
    let cargo_home = target_dir.parent().unwrap_or(target_dir).join("cargo-home");
    environment.insert("CARGO_HOME".to_owned(), cargo_home.display().to_string());
    environment.insert("CARGO_INCREMENTAL".to_owned(), "0".to_owned());
    environment
}

#[must_use]
pub fn find_in_path(tool: &str, environment: &BuildEnvironment) -> Option<PathBuf> {
    environment
        .get("PATH")?
        .split(':')
        .filter(|part| !part.is_empty())
        .map(Path::new)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(tool))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.file_type().is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn set_plain_output(environment: &mut BuildEnvironment) {
    environment.insert("NO_COLOR".to_owned(), "1".to_owned());
    environment.insert("CLICOLOR".to_owned(), "0".to_owned());
    environment.insert("CLICOLOR_FORCE".to_owned(), "0".to_owned());
}

fn push_existing_path(paths: &mut Vec<String>, path: impl AsRef<Path>) {
    let path = path.as_ref();
    if path.is_absolute() && path.is_dir() {
        paths.push(path.display().to_string());
    }
}

fn push_safe_inherited_path(paths: &mut Vec<String>, path: &Path) {
    if !path.is_absolute() {
        return;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let owner = metadata.uid();
        // SAFETY: `geteuid` has no arguments or memory-safety preconditions.
        let current_user = unsafe { libc::geteuid() };
        if metadata.permissions().mode() & 0o022 != 0 || (owner != 0 && owner != current_user) {
            return;
        }
    }

    if let Ok(canonical) = path.canonicalize() {
        paths.push(canonical.display().to_string());
    }
}

fn llvm_candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/opt/homebrew/opt/llvm"),
            PathBuf::from("/usr/local/opt/llvm"),
        ]
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

    #[test]
    fn inherited_build_injection_variables_are_not_forwarded() {
        let dangerous = [
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "url.file:///tmp/fake.insteadOf"),
            (
                "GIT_CONFIG_VALUE_0",
                "https://github.com/bitcoin/bitcoin.git",
            ),
            ("RUSTC_WRAPPER", "/tmp/wrapper"),
            ("RUSTFLAGS", "-C linker=/tmp/linker"),
            ("CMAKE_TOOLCHAIN_FILE", "/tmp/toolchain"),
            ("DYLD_INSERT_LIBRARIES", "/tmp/injected.dylib"),
            ("DYLD_LIBRARY_PATH", "/tmp/injected-libraries"),
            ("LD_LIBRARY_PATH", "/tmp/injected-libraries"),
            ("PATH", "/tmp/injected-build-tools"),
        ];
        let environment = build_environment_from(|name| {
            dangerous
                .iter()
                .find_map(|(candidate, value)| (*candidate == name).then(|| (*value).to_owned()))
        });

        assert_eq!(
            environment.get("GIT_CONFIG_COUNT").map(String::as_str),
            Some("0")
        );
        assert!(!environment
            .get("PATH")
            .is_some_and(|path| path.contains("injected-build-tools")));
        for name in [
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "RUSTC_WRAPPER",
            "RUSTFLAGS",
            "CMAKE_TOOLCHAIN_FILE",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "LD_LIBRARY_PATH",
        ] {
            assert!(!environment.contains_key(name), "unexpected {name}");
        }
    }

    #[test]
    fn cargo_uses_a_job_local_home() {
        let base = BuildEnvironment::new();
        let environment = cargo_environment(&base, Path::new("/tmp/job/electrs-target"));
        assert_eq!(
            environment.get("CARGO_HOME").map(String::as_str),
            Some("/tmp/job/cargo-home")
        );
        assert_eq!(
            environment.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("0")
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_absolute_inherited_path_supports_manual_tool_installations() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))?;
        let tool = temporary.path().join("manual-rustc");
        std::fs::write(&tool, "#!/bin/sh\n")?;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))?;
        let inherited = temporary.path().display().to_string();

        let environment =
            build_environment_from(|name| (name == "PATH").then(|| inherited.clone()));

        assert_eq!(
            find_in_path("manual-rustc", &environment),
            Some(tool.canonicalize()?)
        );
        Ok(())
    }

    #[test]
    fn macos_llvm_candidates_cover_apple_silicon_and_intel_homebrew() {
        if cfg!(target_os = "macos") {
            let candidates = llvm_candidates();
            assert!(candidates.contains(&PathBuf::from("/opt/homebrew/opt/llvm")));
            assert!(candidates.contains(&PathBuf::from("/usr/local/opt/llvm")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_ignores_relative_and_non_executable_entries() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempfile::tempdir()?;
        let executable = temporary.path().join("tool");
        std::fs::write(&executable, "#!/bin/sh\n")?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644))?;
        let mut environment = BuildEnvironment::new();
        environment.insert(
            "PATH".to_owned(),
            format!("relative:{}", temporary.path().display()),
        );
        assert!(find_in_path("tool", &environment).is_none());

        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        assert_eq!(find_in_path("tool", &environment), Some(executable));
        Ok(())
    }
}
