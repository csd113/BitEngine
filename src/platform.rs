//! Platform-specific helpers kept behind a small boundary.

use std::{
    net::TcpListener,
    path::{Component, Path, PathBuf},
    process::Child,
};

use anyhow::{Context as _, Result};

pub const APP_NAME: &str = "BitEngine";

const SINGLE_INSTANCE_ADDR: &str = "127.0.0.1:49382";

#[derive(Debug)]
pub struct SingleInstanceGuard {
    #[allow(dead_code)]
    listener: TcpListener,
}

impl SingleInstanceGuard {
    #[must_use]
    pub fn acquire() -> Option<Self> {
        TcpListener::bind(SINGLE_INSTANCE_ADDR)
            .ok()
            .map(|listener| Self { listener })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOsAppleSilicon,
    LinuxX64,
    LinuxArm64,
    Unsupported,
}

impl Platform {
    #[must_use]
    pub fn current() -> Self {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("macos", "aarch64") => Self::MacOsAppleSilicon,
            ("linux", "x86_64") => Self::LinuxX64,
            ("linux", "aarch64") => Self::LinuxArm64,
            _ => Self::Unsupported,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MacOsAppleSilicon => "macOS Apple Silicon",
            Self::LinuxX64 => "Linux x86_64",
            Self::LinuxArm64 => "Linux ARM64",
            Self::Unsupported => "unsupported platform",
        }
    }
}

#[must_use]
pub fn executable_name(base: &str) -> String {
    base.to_owned()
}

#[must_use]
pub fn electrs_binary_name() -> String {
    executable_name("electrs")
}

#[must_use]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    )
}

pub fn set_executable_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod {}", path.display()))
}

/// Resolve an application-managed directory only after rejecting a symbolic
/// link or non-directory at its final path component.
pub fn prepare_real_directory(path: &Path, label: &str, create: bool) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        anyhow::bail!("unsafe {label} path: {}", path.display());
    }

    let mut created = false;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("{label} must be a real directory: {}", path.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
            std::fs::create_dir_all(path)
                .with_context(|| format!("create {label} {}", path.display()))?;
            created = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("{label} not found at {}", path.display());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {label} {}", path.display()));
        }
    }

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reinspect {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("{label} changed during validation: {}", path.display());
    }
    if created {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure {label} {}", path.display()))?;
    }
    std::fs::canonicalize(path).with_context(|| format!("canonicalize {label} {}", path.display()))
}

pub fn terminate_child(child: &Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: `pid` comes from a live child process owned by this handle,
        // and `kill` only sends a signal to that operating-system process.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

#[must_use]
pub fn command_display(program: &Path, args: &[String]) -> String {
    std::iter::once(shell_display(program))
        .chain(args.iter().map(|arg| shell_display(Path::new(arg))))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_display(value: &Path) -> String {
    let text = value.display().to_string();
    if text.contains(char::is_whitespace) {
        format!("{text:?}")
    } else {
        text
    }
}
