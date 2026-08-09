//! Transactional installation of a verified binary set.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context as _, Result};

#[derive(Debug, Clone)]
pub struct InstallArtifact {
    pub source: PathBuf,
    pub name: String,
}

/// Install all artifacts as a transaction.
///
/// Every source is copied to a job-specific temporary file in the destination
/// filesystem before any existing binary is touched. Existing binaries are
/// renamed to backups, and any later rename failure restores the whole set.
pub fn install_transaction(
    artifacts: &[InstallArtifact],
    destination: &Path,
    transaction_id: &str,
) -> Result<Vec<String>> {
    if artifacts.is_empty() {
        bail!("no verified binaries were provided for installation");
    }
    validate_transaction_id(transaction_id)?;
    fs::create_dir_all(destination)
        .with_context(|| format!("create binaries directory {}", destination.display()))?;

    let mut plans = Vec::with_capacity(artifacts.len());
    let mut required_bytes = 0_u64;
    for artifact in artifacts {
        validate_binary_name(&artifact.name)?;
        let metadata = fs::symlink_metadata(&artifact.source)
            .with_context(|| format!("inspect built binary {}", artifact.source.display()))?;
        if !metadata.file_type().is_file() {
            bail!(
                "built binary is not a regular file: {}",
                artifact.source.display()
            );
        }
        required_bytes = required_bytes.saturating_add(metadata.len());
        plans.push(InstallPlan {
            source: artifact.source.clone(),
            destination: destination.join(&artifact.name),
            temporary: destination
                .join(format!(".bitengine-{transaction_id}-{}.tmp", artifact.name)),
            backup: destination.join(format!(
                ".bitengine-{transaction_id}-{}.backup",
                artifact.name
            )),
            name: artifact.name.clone(),
            expected_size: metadata.len(),
        });
    }

    if let Some(available) = available_space_bytes(destination) {
        let safety_margin = 64 * 1024 * 1024_u64;
        if available < required_bytes.saturating_add(safety_margin) {
            bail!(
                "not enough free space to install binaries safely (need at least {}, available {})",
                format_bytes(required_bytes.saturating_add(safety_margin)),
                format_bytes(available)
            );
        }
    }

    for plan in &plans {
        remove_if_exists(&plan.temporary)?;
        remove_if_exists(&plan.backup)?;
    }

    if let Err(error) = stage_all(&plans) {
        cleanup_temporary(&plans);
        return Err(error);
    }

    let mut backed_up = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        if plan.destination.exists() {
            if let Err(error) = fs::rename(&plan.destination, &plan.backup) {
                restore_backups(&plans, &backed_up);
                cleanup_temporary(&plans);
                return Err(error).with_context(|| {
                    format!(
                        "prepare existing {} for replacement",
                        plan.destination.display()
                    )
                });
            }
            backed_up.push(index);
        }
    }

    let mut installed: Vec<usize> = Vec::new();
    for (index, plan) in plans.iter().enumerate() {
        if let Err(error) = fs::rename(&plan.temporary, &plan.destination) {
            for installed_index in installed.iter().copied() {
                let _ = fs::remove_file(&plans[installed_index].destination);
            }
            restore_backups(&plans, &backed_up);
            cleanup_temporary(&plans);
            return Err(error).with_context(|| {
                format!("activate verified binary {}", plan.destination.display())
            });
        }
        installed.push(index);
    }

    sync_directory(destination);
    for index in backed_up {
        // The transaction has committed at this point. A stale backup is less
        // harmful than reporting failure after the new set is already active.
        let _ = fs::remove_file(&plans[index].backup);
    }
    sync_directory(destination);

    Ok(plans.into_iter().map(|plan| plan.name).collect())
}

#[derive(Debug)]
struct InstallPlan {
    source: PathBuf,
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    name: String,
    expected_size: u64,
}

fn stage_all(plans: &[InstallPlan]) -> Result<()> {
    for plan in plans {
        let copied = fs::copy(&plan.source, &plan.temporary).with_context(|| {
            format!(
                "stage {} as {}",
                plan.source.display(),
                plan.temporary.display()
            )
        })?;
        if copied != plan.expected_size {
            bail!(
                "staged binary size mismatch for {} (expected {}, copied {})",
                plan.name,
                plan.expected_size,
                copied
            );
        }
        crate::platform::set_executable_permissions(&plan.temporary)?;
        fs::File::open(&plan.temporary)
            .with_context(|| format!("open staged binary {}", plan.temporary.display()))?
            .sync_all()
            .with_context(|| format!("sync staged binary {}", plan.temporary.display()))?;
    }
    Ok(())
}

fn restore_backups(plans: &[InstallPlan], backed_up: &[usize]) {
    for index in backed_up.iter().rev().copied() {
        let plan = &plans[index];
        let _ = fs::rename(&plan.backup, &plan.destination);
    }
}

fn cleanup_temporary(plans: &[InstallPlan]) {
    for plan in plans {
        let _ = fs::remove_file(&plan.temporary);
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove stale file {}", path.display())),
    }
}

fn validate_binary_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || Path::new(name).file_name().is_none()
    {
        bail!("unsafe binary filename: {name:?}");
    }
    Ok(())
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.is_empty()
        || !transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("unsafe installation transaction identifier");
    }
    Ok(())
}

pub fn available_space_bytes(path: &Path) -> Option<u64> {
    let existing = nearest_existing_ancestor(path)?;
    let output = Command::new("df").arg("-Pk").arg(existing).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_available_bytes(&String::from_utf8(output.stdout).ok()?)
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
}

fn parse_df_available_bytes(output: &str) -> Option<u64> {
    let line = output.lines().rfind(|line| !line.trim().is_empty())?;
    let blocks = line.split_whitespace().nth(3)?.parse::<u64>().ok()?;
    blocks.checked_mul(1024)
}

pub(super) fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format_decimal_units(bytes, GIB, "GiB")
    } else {
        format_decimal_units(bytes, MIB, "MiB")
    }
}

fn format_decimal_units(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let tenths = bytes % unit * 10 / unit;
    format!("{whole}.{tenths} {suffix}")
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = fs::File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
        let mut file = fs::File::create(path)?;
        file.write_all(contents)?;
        Ok(())
    }

    #[test]
    fn successful_install_replaces_a_complete_binary_set() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        write_file(&source.join("bitcoin-cli"), b"new cli")?;
        write_file(&destination.join("bitcoind"), b"old bitcoind")?;
        write_file(&destination.join("bitcoin-cli"), b"old cli")?;

        let installed = install_transaction(
            &[
                InstallArtifact {
                    source: source.join("bitcoind"),
                    name: "bitcoind".to_owned(),
                },
                InstallArtifact {
                    source: source.join("bitcoin-cli"),
                    name: "bitcoin-cli".to_owned(),
                },
            ],
            &destination,
            "test-success",
        )?;

        assert_eq!(installed, vec!["bitcoind", "bitcoin-cli"]);
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"new cli");
        Ok(())
    }

    #[test]
    fn staging_failure_preserves_every_existing_binary() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        write_file(&destination.join("bitcoind"), b"old bitcoind")?;
        write_file(&destination.join("bitcoin-cli"), b"old cli")?;

        let result = install_transaction(
            &[
                InstallArtifact {
                    source: source.join("bitcoind"),
                    name: "bitcoind".to_owned(),
                },
                InstallArtifact {
                    source: source.join("missing-cli"),
                    name: "bitcoin-cli".to_owned(),
                },
            ],
            &destination,
            "test-failure",
        );

        assert!(result.is_err());
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
        Ok(())
    }

    #[test]
    fn df_output_parser_reads_available_kibibytes() {
        let output = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk 1000 250 750 25% /tmp\n";
        assert_eq!(parse_df_available_bytes(output), Some(750 * 1024));
    }
}
