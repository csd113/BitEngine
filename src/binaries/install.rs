//! Crash-safe transactional installation of a verified binary set.

use std::{
    collections::HashSet,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    mem::MaybeUninit,
    os::unix::{
        ffi::OsStrExt as _,
        fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
        io::AsRawFd as _,
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};

const INSTALL_LOCK_NAME: &str = ".bitengine-install.lock";
const INSTALL_JOURNAL_NAME: &str = ".bitengine-install.json";
const INSTALL_JOURNAL_TEMP_NAME: &str = ".bitengine-install.json.tmp";
const INSTALL_JOURNAL_VERSION: u32 = 1;
const MAX_MANAGED_ARTIFACTS: usize = 32;
const MAX_JOURNAL_BYTES: u64 = 1024 * 1024;
const MAX_PATH_TOKEN_BYTES: usize = 96;

#[derive(Debug, Clone)]
pub struct InstallArtifact {
    pub source: PathBuf,
    pub name: String,
}

/// Result of repairing a durable installation journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallRecovery {
    /// No pending transaction existed.
    None,
    /// An uncommitted transaction was rolled back to the complete old set.
    RolledBack,
    /// A committed transaction was validated and its stale backups were removed.
    Finalized,
}

/// Install a complete managed executable family as a crash-safe transaction.
///
/// Every managed name absent from `artifacts` is a tombstone: an older installed
/// file with that name is backed up and removed on commit, or restored on
/// rollback. A durable journal is activated before any existing binary is
/// renamed, allowing [`recover_pending_install`] to repair interruption at any
/// later filesystem boundary.
pub fn install_transaction_managed(
    artifacts: &[InstallArtifact],
    managed_names: &[&str],
    destination: &Path,
    transaction_id: &str,
) -> Result<Vec<String>> {
    validate_transaction_id(transaction_id)?;
    let destination = prepare_destination(destination, true)?
        .context("created installation destination was not available")?;
    let _lock = DestinationLock::acquire(&destination)?;
    recover_pending_locked(&destination)?;

    let mut plans = build_plans(artifacts, managed_names, &destination, transaction_id)?;
    check_install_space(&plans, &destination)?;

    if let Err(error) = stage_all(&mut plans) {
        return Err(with_cleanup_error(error, cleanup_unjournaled(&plans)));
    }
    if let Err(error) = validate_prepared_inputs(&plans) {
        return Err(with_cleanup_error(error, cleanup_unjournaled(&plans)));
    }

    let mut journal = InstallJournal::from_plans(transaction_id, &plans)?;
    if let Err(error) = write_journal(&destination, &journal, JournalWrite::Create) {
        return fail_before_mutation(&destination, &plans, error);
    }

    let mutation = backup_all(&plans)
        .and_then(|()| activate_all(&plans))
        .and_then(|()| sync_directory(&destination));
    if let Err(error) = mutation {
        return fail_and_recover_prepared(&destination, &error);
    }

    journal.phase = JournalPhase::Committed;
    if let Err(error) = write_journal(&destination, &journal, JournalWrite::Replace) {
        return match recover_pending_locked(&destination) {
            Ok(InstallRecovery::Finalized) => Ok(installed_names(artifacts)),
            Ok(InstallRecovery::RolledBack) => Err(anyhow!(
                "could not durably commit the installation: {error:#}; the old binary set was restored"
            )),
            Ok(InstallRecovery::None) => Err(anyhow!(
                "could not durably commit the installation: {error:#}; the transaction journal disappeared"
            )),
            Err(recovery_error) => Err(anyhow!(
                "could not durably commit the installation: {error:#}; automatic recovery failed and the journal was retained: {recovery_error:#}"
            )),
        };
    }

    match recover_pending_locked(&destination) {
        Ok(InstallRecovery::Finalized) => Ok(installed_names(artifacts)),
        Ok(other) => bail!(
            "committed installation produced an unexpected recovery result: {other:?}"
        ),
        Err(error) => Err(error).context(
            "the new binaries were committed, but durable transaction cleanup did not complete; the journal was retained",
        ),
    }
}

/// Repair a pending installation without starting a new build.
///
/// Recovery is serialized by the same destination lock used for installation.
/// Ambiguous or non-regular entries fail closed and leave the journal in place.
pub fn recover_pending_install(destination: &Path) -> Result<InstallRecovery> {
    let Some(destination) = prepare_destination(destination, false)? else {
        return Ok(InstallRecovery::None);
    };
    let _lock = DestinationLock::acquire(&destination)?;
    recover_pending_locked(&destination)
}

fn installed_names(artifacts: &[InstallArtifact]) -> Vec<String> {
    artifacts
        .iter()
        .map(|artifact| artifact.name.clone())
        .collect()
}

#[derive(Debug)]
struct InstallPlan {
    source: Option<PathBuf>,
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
    name: String,
    expected_size: Option<u64>,
    source_identity: Option<FileIdentity>,
    original: Option<FileIdentity>,
    staged: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            mode: metadata.mode() & 0o7777,
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        self == Self::from_metadata(metadata)
    }

    fn same_file(self, metadata: &fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum JournalPhase {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalArtifact {
    name: String,
    original: Option<FileIdentity>,
    staged: Option<FileIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallJournal {
    version: u32,
    transaction_id: String,
    phase: JournalPhase,
    artifacts: Vec<JournalArtifact>,
}

impl InstallJournal {
    fn from_plans(transaction_id: &str, plans: &[InstallPlan]) -> Result<Self> {
        let journal = Self {
            version: INSTALL_JOURNAL_VERSION,
            transaction_id: transaction_id.to_owned(),
            phase: JournalPhase::Prepared,
            artifacts: plans
                .iter()
                .map(|plan| JournalArtifact {
                    name: plan.name.clone(),
                    original: plan.original,
                    staged: plan.staged,
                })
                .collect(),
        };
        validate_journal(&journal)?;
        Ok(journal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalWrite {
    Create,
    Replace,
}

#[derive(Debug)]
struct DestinationLock {
    file: File,
}

impl DestinationLock {
    fn acquire(destination: &Path) -> Result<Self> {
        let path = destination.join(INSTALL_LOCK_NAME);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("open installation lock {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect installation lock {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!(
                "installation lock is not a regular file: {}",
                path.display()
            );
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure installation lock {}", path.display()))?;

        // SAFETY: `file` owns this live file descriptor for the lifetime of the
        // guard, and `flock` does not dereference any Rust memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            return Err(error).with_context(|| {
                format!(
                    "lock installation destination {}; another installer may be active",
                    destination.display()
                )
            });
        }
        Ok(Self { file })
    }
}

impl Drop for DestinationLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains owned by `self.file` until this drop
        // implementation returns. Closing it would also release the lock.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn prepare_destination(path: &Path, create: bool) -> Result<Option<PathBuf>> {
    validate_destination_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_destination_root(path, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_destination_durably(path)?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect binaries directory {}", path.display()));
        }
    }

    let canonical = fs::canonicalize(path)
        .with_context(|| format!("canonicalize binaries directory {}", path.display()))?;
    let metadata = fs::symlink_metadata(&canonical)
        .with_context(|| format!("inspect binaries directory {}", canonical.display()))?;
    validate_destination_root(&canonical, &metadata)?;
    Ok(Some(canonical))
}

fn validate_destination_path(path: &Path) -> Result<()> {
    if !path.is_absolute() || path.parent().is_none() {
        bail!(
            "binaries destination must be an absolute non-root path: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        bail!(
            "binaries destination must not contain . or ..: {}",
            path.display()
        );
    }
    Ok(())
}

fn create_destination_durably(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.to_path_buf());
                ancestor = ancestor.parent().with_context(|| {
                    format!(
                        "find existing parent for binaries directory {}",
                        path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect binaries parent {}", ancestor.display()));
            }
        }
    }

    let canonical_ancestor = fs::canonicalize(ancestor)
        .with_context(|| format!("canonicalize binaries parent {}", ancestor.display()))?;
    let ancestor_metadata = fs::symlink_metadata(&canonical_ancestor)
        .with_context(|| format!("inspect binaries parent {}", canonical_ancestor.display()))?;
    if !ancestor_metadata.is_dir() {
        bail!(
            "binaries destination parent is not a directory: {}",
            ancestor.display()
        );
    }

    for directory in missing.into_iter().rev() {
        fs::create_dir(&directory)
            .with_context(|| format!("create binaries directory {}", directory.display()))?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("secure binaries directory {}", directory.display()))?;
        let metadata = fs::symlink_metadata(&directory)
            .with_context(|| format!("inspect binaries directory {}", directory.display()))?;
        validate_destination_root(&directory, &metadata)?;
        let parent = directory.parent().with_context(|| {
            format!("binaries directory has no parent: {}", directory.display())
        })?;
        sync_directory(parent).with_context(|| {
            format!(
                "durably create binaries directory entry {}",
                directory.display()
            )
        })?;
    }
    Ok(())
}

fn validate_destination_root(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "binaries destination must be a real directory, not {}: {}",
            file_type_label(metadata),
            path.display()
        );
    }
    Ok(())
}

fn build_plans(
    artifacts: &[InstallArtifact],
    managed_names: &[&str],
    destination: &Path,
    transaction_id: &str,
) -> Result<Vec<InstallPlan>> {
    if artifacts.is_empty() {
        bail!("no verified binaries were provided for installation");
    }
    if managed_names.is_empty() || managed_names.len() > MAX_MANAGED_ARTIFACTS {
        bail!("invalid managed binary set size");
    }

    let mut managed = HashSet::with_capacity(managed_names.len());
    for &name in managed_names {
        validate_binary_name(name)?;
        if !managed.insert(name) {
            bail!("duplicate managed binary filename: {name:?}");
        }
    }

    let mut artifact_names = HashSet::with_capacity(artifacts.len());
    for artifact in artifacts {
        validate_binary_name(&artifact.name)?;
        if !artifact_names.insert(artifact.name.as_str()) {
            bail!("duplicate installation artifact: {:?}", artifact.name);
        }
        if !managed.contains(artifact.name.as_str()) {
            bail!(
                "installation artifact is not in the managed binary set: {:?}",
                artifact.name
            );
        }
    }

    let mut plans = Vec::with_capacity(managed_names.len());
    for &name in managed_names {
        let artifact = artifacts.iter().find(|artifact| artifact.name == name);
        let (source, expected_size, source_identity) = if let Some(artifact) = artifact {
            let metadata = fs::symlink_metadata(&artifact.source)
                .with_context(|| format!("inspect built binary {}", artifact.source.display()))?;
            if !metadata.file_type().is_file() {
                bail!(
                    "built binary is not a regular file: {}",
                    artifact.source.display()
                );
            }
            if metadata.len() == 0 {
                bail!("built binary is empty: {}", artifact.source.display());
            }
            (
                Some(artifact.source.clone()),
                Some(metadata.len()),
                Some(FileIdentity::from_metadata(&metadata)),
            )
        } else {
            (None, None, None)
        };

        let installed = destination.join(name);
        let original = inspect_optional_regular(&installed, "installed binary")?;
        let temporary = destination.join(format!(".bitengine-{transaction_id}-{name}.tmp"));
        let backup = destination.join(format!(".bitengine-{transaction_id}-{name}.backup"));
        require_absent(&temporary, "transaction staging entry")?;
        require_absent(&backup, "transaction backup entry")?;
        plans.push(InstallPlan {
            source,
            destination: installed,
            temporary,
            backup,
            name: name.to_owned(),
            expected_size,
            source_identity,
            original,
            staged: None,
        });
    }
    Ok(plans)
}

fn check_install_space(plans: &[InstallPlan], destination: &Path) -> Result<()> {
    let required_bytes = plans.iter().fold(0_u64, |total, plan| {
        total.saturating_add(plan.expected_size.unwrap_or(0))
    });
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
    Ok(())
}

fn stage_all(plans: &mut [InstallPlan]) -> Result<()> {
    for plan in plans {
        let Some(source_path) = plan.source.as_ref() else {
            continue;
        };
        let expected_size = plan
            .expected_size
            .context("staged artifact had no expected size")?;
        let source_identity = plan
            .source_identity
            .context("staged artifact had no source identity")?;
        let mut source = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(source_path)
            .with_context(|| format!("open built binary {}", source_path.display()))?;
        let source_metadata = source
            .metadata()
            .with_context(|| format!("inspect built binary {}", source_path.display()))?;
        if !source_metadata.file_type().is_file() || !source_identity.matches(&source_metadata) {
            bail!(
                "built binary changed before staging: {}",
                source_path.display()
            );
        }

        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&plan.temporary)
            .with_context(|| format!("create staged binary {}", plan.temporary.display()))?;
        plan.staged = Some(FileIdentity::from_metadata(
            &temporary
                .metadata()
                .with_context(|| format!("inspect staged binary {}", plan.temporary.display()))?,
        ));

        let copied = std::io::copy(&mut source, &mut temporary).with_context(|| {
            format!(
                "stage {} as {}",
                source_path.display(),
                plan.temporary.display()
            )
        })?;
        crate::platform::set_executable_permissions(&plan.temporary)?;
        temporary
            .sync_all()
            .with_context(|| format!("sync staged binary {}", plan.temporary.display()))?;
        let staged_metadata = temporary
            .metadata()
            .with_context(|| format!("inspect staged binary {}", plan.temporary.display()))?;
        plan.staged = Some(FileIdentity::from_metadata(&staged_metadata));
        if copied != expected_size || staged_metadata.len() != expected_size {
            bail!(
                "staged binary size mismatch for {} (expected {}, copied {})",
                plan.name,
                expected_size,
                copied
            );
        }
        let final_source_metadata = source
            .metadata()
            .with_context(|| format!("reinspect built binary {}", source_path.display()))?;
        if !source_identity.matches(&final_source_metadata) {
            bail!(
                "built binary changed while it was being staged: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn validate_prepared_inputs(plans: &[InstallPlan]) -> Result<()> {
    for plan in plans {
        match plan.original {
            Some(identity) => require_identity(&plan.destination, identity, "installed binary")?,
            None => require_absent(&plan.destination, "installed binary")?,
        }
        require_absent(&plan.backup, "transaction backup entry")?;
        match plan.staged {
            Some(identity) => require_identity(&plan.temporary, identity, "staged binary")?,
            None => require_absent(&plan.temporary, "transaction staging entry")?,
        }
    }
    Ok(())
}

fn backup_all(plans: &[InstallPlan]) -> Result<()> {
    for plan in plans {
        backup_one(plan)?;
    }
    Ok(())
}

fn backup_one(plan: &InstallPlan) -> Result<()> {
    require_absent(&plan.backup, "transaction backup entry")?;
    if let Some(identity) = plan.original {
        require_identity(&plan.destination, identity, "installed binary")?;
        fs::rename(&plan.destination, &plan.backup).with_context(|| {
            format!(
                "prepare existing {} for replacement",
                plan.destination.display()
            )
        })?;
    } else {
        require_absent(&plan.destination, "installed binary")?;
    }
    Ok(())
}

fn activate_all(plans: &[InstallPlan]) -> Result<()> {
    for plan in plans {
        activate_one(plan)?;
    }
    Ok(())
}

fn activate_one(plan: &InstallPlan) -> Result<()> {
    require_absent(&plan.destination, "installation destination")?;
    if let Some(identity) = plan.staged {
        require_identity(&plan.temporary, identity, "staged binary")?;
        fs::rename(&plan.temporary, &plan.destination)
            .with_context(|| format!("activate verified binary {}", plan.destination.display()))?;
    } else {
        require_absent(&plan.temporary, "transaction staging entry")?;
    }
    Ok(())
}

fn fail_before_mutation(
    destination: &Path,
    plans: &[InstallPlan],
    error: anyhow::Error,
) -> Result<Vec<String>> {
    let active_journal = destination.join(INSTALL_JOURNAL_NAME);
    match fs::symlink_metadata(&active_journal) {
        Err(inspect_error) if inspect_error.kind() == std::io::ErrorKind::NotFound => {
            let temporary_journal = destination.join(INSTALL_JOURNAL_TEMP_NAME);
            let cleanup = remove_internal_file_if_exists(
                &temporary_journal,
                "temporary installation journal",
            )
            .and_then(|()| cleanup_unjournaled(plans))
            .and_then(|()| sync_directory(destination));
            return Err(with_cleanup_error(error, cleanup));
        }
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(metadata) => {
            return Err(anyhow!(
                "could not durably prepare installation: {error:#}; active journal became {} and transaction state was retained",
                file_type_label(&metadata)
            ));
        }
        Err(inspect_error) => {
            return Err(anyhow!(
                "could not durably prepare installation: {error:#}; could not inspect transaction state: {inspect_error}"
            ));
        }
    }

    match recover_pending_locked(destination) {
        Ok(InstallRecovery::RolledBack) => Err(error).context(
            "could not durably prepare installation; staged files were cleaned up",
        ),
        Ok(InstallRecovery::None) => Err(with_cleanup_error(error, cleanup_unjournaled(plans))),
        Ok(InstallRecovery::Finalized) => Err(error)
            .context("unexpected committed journal appeared while preparing installation"),
        Err(recovery_error) => Err(anyhow!(
            "could not durably prepare installation: {error:#}; automatic cleanup failed and transaction state was retained: {recovery_error:#}"
        )),
    }
}

fn fail_and_recover_prepared(destination: &Path, error: &anyhow::Error) -> Result<Vec<String>> {
    match recover_pending_locked(destination) {
        Ok(InstallRecovery::RolledBack) => Err(anyhow!(
            "installation failed and the complete old binary set was restored: {error:#}"
        )),
        Ok(InstallRecovery::Finalized) => Err(anyhow!(
            "installation failed after the transaction was already committed: {error:#}"
        )),
        Ok(InstallRecovery::None) => Err(anyhow!(
            "installation failed and its durable journal disappeared: {error:#}"
        )),
        Err(recovery_error) => Err(anyhow!(
            "installation failed: {error:#}; automatic rollback failed and the journal was retained: {recovery_error:#}"
        )),
    }
}

fn recover_pending_locked(destination: &Path) -> Result<InstallRecovery> {
    let journal_path = destination.join(INSTALL_JOURNAL_NAME);
    let journal_temp_path = destination.join(INSTALL_JOURNAL_TEMP_NAME);
    let journal = if let Some(journal) = read_journal(&journal_path)? {
        remove_internal_file_if_exists(&journal_temp_path, "temporary installation journal")?;
        journal
    } else {
        let Some(journal) = read_journal(&journal_temp_path)? else {
            return Ok(InstallRecovery::None);
        };
        fs::rename(&journal_temp_path, &journal_path).with_context(|| {
            format!(
                "activate recoverable installation journal {}",
                journal_path.display()
            )
        })?;
        sync_directory(destination)?;
        journal
    };

    match journal.phase {
        JournalPhase::Prepared => recover_prepared(destination, &journal),
        JournalPhase::Committed => recover_committed(destination, &journal),
    }
}

fn recover_prepared(destination: &Path, journal: &InstallJournal) -> Result<InstallRecovery> {
    validate_prepared_recovery(destination, journal)?;

    for artifact in &journal.artifacts {
        let paths = JournalPaths::new(destination, &journal.transaction_id, &artifact.name);
        let backup = inspect_optional_regular(&paths.backup, "transaction backup")?;
        let installed = inspect_optional_regular(&paths.destination, "installation destination")?;

        if let Some(original) = artifact.original {
            if backup.is_some() {
                if let Some(installed_identity) = installed {
                    let staged = artifact
                        .staged
                        .context("prepared destination had no staged identity")?;
                    if installed_identity != staged {
                        bail!(
                            "refusing to remove unexpected file while restoring {}",
                            paths.destination.display()
                        );
                    }
                    remove_matching_file(&paths.destination, staged, "staged destination")?;
                }
                require_identity(&paths.backup, original, "transaction backup")?;
                fs::rename(&paths.backup, &paths.destination).with_context(|| {
                    format!("restore binary backup {}", paths.destination.display())
                })?;
            }
        } else if let Some(installed_identity) = installed {
            let staged = artifact
                .staged
                .context("new destination had no staged identity")?;
            if installed_identity != staged {
                bail!(
                    "refusing to remove unexpected file while rolling back {}",
                    paths.destination.display()
                );
            }
            remove_matching_file(&paths.destination, staged, "staged destination")?;
        }

        if let Some(staged) = artifact.staged {
            remove_matching_file_if_exists(&paths.temporary, staged, "staged binary")?;
        }
    }

    validate_rolled_back(destination, journal)?;
    sync_directory(destination)?;
    remove_active_journal(destination)?;
    Ok(InstallRecovery::RolledBack)
}

fn validate_prepared_recovery(destination: &Path, journal: &InstallJournal) -> Result<()> {
    for artifact in &journal.artifacts {
        let paths = JournalPaths::new(destination, &journal.transaction_id, &artifact.name);
        let installed = inspect_optional_regular(&paths.destination, "installation destination")?;
        let temporary = inspect_optional_regular(&paths.temporary, "staged binary")?;
        let backup = inspect_optional_regular(&paths.backup, "transaction backup")?;

        if let Some(staged) = artifact.staged {
            if temporary.is_some_and(|identity| identity != staged)
                || installed.is_some_and(|identity| {
                    artifact.original != Some(identity) && identity != staged
                })
            {
                bail!(
                    "prepared transaction contains an unexpected file for {}",
                    artifact.name
                );
            }
            if temporary.is_some() && installed == Some(staged) {
                bail!(
                    "prepared transaction has duplicate staged entries for {}",
                    artifact.name
                );
            }
        } else if temporary.is_some() {
            bail!(
                "tombstoned binary has an unexpected staged file: {}",
                artifact.name
            );
        }

        if let Some(original) = artifact.original {
            if backup.is_some_and(|identity| identity != original) {
                bail!(
                    "transaction backup does not match the original {}",
                    artifact.name
                );
            }
            if backup.is_none() && installed != Some(original) {
                bail!(
                    "original {} is neither installed nor recoverable from backup",
                    artifact.name
                );
            }
            if backup.is_some()
                && installed.is_some_and(|identity| Some(identity) != artifact.staged)
            {
                bail!(
                    "installation destination is ambiguous while restoring {}",
                    artifact.name
                );
            }
        } else {
            if backup.is_some() {
                bail!(
                    "new binary has an unexpected transaction backup: {}",
                    artifact.name
                );
            }
            if installed.is_some_and(|identity| Some(identity) != artifact.staged) {
                bail!(
                    "new binary destination contains an unexpected file: {}",
                    artifact.name
                );
            }
        }
    }
    Ok(())
}

fn validate_rolled_back(destination: &Path, journal: &InstallJournal) -> Result<()> {
    for artifact in &journal.artifacts {
        let paths = JournalPaths::new(destination, &journal.transaction_id, &artifact.name);
        match artifact.original {
            Some(identity) => {
                require_identity(&paths.destination, identity, "restored binary")?;
            }
            None => require_absent(&paths.destination, "rolled-back destination")?,
        }
        require_absent(&paths.backup, "transaction backup")?;
        require_absent(&paths.temporary, "staged binary")?;
    }
    Ok(())
}

fn recover_committed(destination: &Path, journal: &InstallJournal) -> Result<InstallRecovery> {
    validate_committed(destination, journal)?;
    for artifact in &journal.artifacts {
        let paths = JournalPaths::new(destination, &journal.transaction_id, &artifact.name);
        if let Some(original) = artifact.original {
            remove_matching_file_if_exists(&paths.backup, original, "transaction backup")?;
        }
        if let Some(staged) = artifact.staged {
            remove_matching_file_if_exists(&paths.temporary, staged, "staged binary")?;
        }
    }
    validate_committed(destination, journal)?;
    sync_directory(destination)?;
    remove_active_journal(destination)?;
    Ok(InstallRecovery::Finalized)
}

fn validate_committed(destination: &Path, journal: &InstallJournal) -> Result<()> {
    for artifact in &journal.artifacts {
        let paths = JournalPaths::new(destination, &journal.transaction_id, &artifact.name);
        if let Some(identity) = artifact.staged {
            require_identity(&paths.destination, identity, "committed binary")?;
            if inspect_optional_regular(&paths.temporary, "staged binary")?.is_some() {
                bail!(
                    "committed transaction retained an unexpected staged copy of {}",
                    artifact.name
                );
            }
        } else {
            require_absent(&paths.destination, "committed tombstone destination")?;
            require_absent(&paths.temporary, "tombstone staging entry")?;
        }
        match artifact.original {
            Some(identity) => {
                if inspect_optional_regular(&paths.backup, "transaction backup")?
                    .is_some_and(|backup| backup != identity)
                {
                    bail!(
                        "committed transaction backup does not match original {}",
                        artifact.name
                    );
                }
            }
            None => require_absent(&paths.backup, "unexpected transaction backup")?,
        }
    }
    Ok(())
}

#[derive(Debug)]
struct JournalPaths {
    destination: PathBuf,
    temporary: PathBuf,
    backup: PathBuf,
}

impl JournalPaths {
    fn new(destination: &Path, transaction_id: &str, name: &str) -> Self {
        Self {
            destination: destination.join(name),
            temporary: destination.join(format!(".bitengine-{transaction_id}-{name}.tmp")),
            backup: destination.join(format!(".bitengine-{transaction_id}-{name}.backup")),
        }
    }
}

fn write_journal(destination: &Path, journal: &InstallJournal, write: JournalWrite) -> Result<()> {
    validate_journal(journal)?;
    let active = destination.join(INSTALL_JOURNAL_NAME);
    let temporary = destination.join(INSTALL_JOURNAL_TEMP_NAME);
    match write {
        JournalWrite::Create => {
            if journal.phase != JournalPhase::Prepared {
                bail!("new installation journal must begin in the prepared phase");
            }
            require_absent(&active, "installation journal")?;
        }
        JournalWrite::Replace => {
            let current = read_journal(&active)?
                .context("prepared installation journal disappeared before commit")?;
            if current.version != journal.version
                || current.transaction_id != journal.transaction_id
                || current.phase != JournalPhase::Prepared
                || current.artifacts != journal.artifacts
                || journal.phase != JournalPhase::Committed
            {
                bail!("prepared installation journal changed before commit");
            }
        }
    }
    require_absent(&temporary, "temporary installation journal")?;

    let json = serde_json::to_vec_pretty(journal).context("serialize installation journal")?;
    if u64::try_from(json.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        bail!("installation journal is unexpectedly large");
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("create installation journal {}", temporary.display()))?;
    file.write_all(&json)
        .with_context(|| format!("write installation journal {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("sync installation journal {}", temporary.display()))?;
    fs::rename(&temporary, &active)
        .with_context(|| format!("activate installation journal {}", active.display()))?;
    sync_directory(destination)
}

fn read_journal(path: &Path) -> Result<Option<InstallJournal>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open installation journal {}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect installation journal {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "installation journal is not a regular file: {}",
            path.display()
        );
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("installation journal is too large: {}", path.display());
    }
    let capacity = usize::try_from(metadata.len()).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_JOURNAL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read installation journal {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_JOURNAL_BYTES {
        bail!("installation journal is too large: {}", path.display());
    }
    let journal = serde_json::from_slice::<InstallJournal>(&bytes)
        .with_context(|| format!("parse installation journal {}", path.display()))?;
    validate_journal(&journal)?;
    Ok(Some(journal))
}

fn validate_journal(journal: &InstallJournal) -> Result<()> {
    if journal.version != INSTALL_JOURNAL_VERSION {
        bail!(
            "unsupported installation journal version: {}",
            journal.version
        );
    }
    validate_transaction_id(&journal.transaction_id)?;
    if journal.artifacts.is_empty() || journal.artifacts.len() > MAX_MANAGED_ARTIFACTS {
        bail!("invalid installation journal artifact count");
    }
    let mut names = HashSet::with_capacity(journal.artifacts.len());
    let mut staged_count = 0_usize;
    for artifact in &journal.artifacts {
        validate_binary_name(&artifact.name)?;
        if !names.insert(artifact.name.as_str()) {
            bail!(
                "duplicate binary in installation journal: {:?}",
                artifact.name
            );
        }
        if artifact.staged.is_some() {
            staged_count = staged_count.saturating_add(1);
        }
    }
    if staged_count == 0 {
        bail!("installation journal contains no new binaries");
    }
    Ok(())
}

fn cleanup_unjournaled(plans: &[InstallPlan]) -> Result<()> {
    let mut errors = Vec::new();
    for plan in plans {
        let Some(identity) = plan.staged else {
            continue;
        };
        if let Err(error) = remove_same_file_if_exists(&plan.temporary, identity, "staged binary") {
            errors.push(format!("{error:#}"));
        }
    }
    errors_to_result("clean staged binaries", &errors)
}

fn remove_matching_file(path: &Path, identity: FileIdentity, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} {}", path.display()))?;
    if !metadata.file_type().is_file() || !identity.matches(&metadata) {
        bail!(
            "refusing to remove unexpected {description}: {}",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("remove {description} {}", path.display()))
}

fn remove_matching_file_if_exists(
    path: &Path,
    identity: FileIdentity,
    description: &str,
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || !identity.matches(&metadata) {
                bail!(
                    "refusing to remove unexpected {description}: {}",
                    path.display()
                );
            }
            fs::remove_file(path)
                .with_context(|| format!("remove {description} {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} {}", path.display()))
        }
    }
}

fn remove_same_file_if_exists(
    path: &Path,
    identity: FileIdentity,
    description: &str,
) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || !identity.same_file(&metadata) {
                bail!(
                    "refusing to remove unexpected {description}: {}",
                    path.display()
                );
            }
            fs::remove_file(path)
                .with_context(|| format!("remove {description} {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} {}", path.display()))
        }
    }
}

fn remove_internal_file_if_exists(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!("{description} is not a regular file: {}", path.display());
            }
            fs::remove_file(path)
                .with_context(|| format!("remove {description} {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} {}", path.display()))
        }
    }
}

fn remove_active_journal(destination: &Path) -> Result<()> {
    let journal = destination.join(INSTALL_JOURNAL_NAME);
    remove_internal_file_if_exists(&journal, "installation journal")?;
    finish_journal_removal(destination, &journal, sync_directory)
}

fn finish_journal_removal(
    destination: &Path,
    journal: &Path,
    sync: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    match sync(destination) {
        Ok(()) => Ok(()),
        Err(error) => match fs::symlink_metadata(journal) {
            // The managed file set and its containing directory were already
            // synced before journal removal. If this final cleanup sync fails,
            // the live namespace is nevertheless a complete old or new set.
            // A crash may resurrect the idempotent journal, which startup can
            // safely replay; reporting the transaction as uncommitted here
            // would be false.
            Err(inspect_error) if inspect_error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(error).context("sync installation journal removal"),
            Err(inspect_error) => Err(error).context(format!(
                "sync installation journal removal; could not inspect journal afterward: {inspect_error}"
            )),
        },
    }
}

fn inspect_optional_regular(path: &Path, description: &str) -> Result<Option<FileIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            Ok(Some(FileIdentity::from_metadata(&metadata)))
        }
        Ok(metadata) => bail!(
            "{description} must be a regular file, not {}: {}",
            file_type_label(&metadata),
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} {}", path.display()))
        }
    }
}

fn require_identity(path: &Path, identity: FileIdentity, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} {}", path.display()))?;
    if !metadata.file_type().is_file() || !identity.matches(&metadata) {
        bail!("{description} changed unexpectedly: {}", path.display());
    }
    Ok(())
}

fn require_absent(path: &Path, description: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) => bail!(
            "{description} already exists as {}: {}",
            file_type_label(&metadata),
            path.display()
        ),
        Err(error) => {
            Err(error).with_context(|| format!("inspect {description} {}", path.display()))
        }
    }
}

fn file_type_label(metadata: &fs::Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        "a symbolic link"
    } else if file_type.is_dir() {
        "a directory"
    } else if file_type.is_file() {
        "a regular file"
    } else {
        "a special filesystem entry"
    }
}

fn with_cleanup_error(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => anyhow!("{error:#}; cleanup also failed: {cleanup_error:#}"),
    }
}

fn errors_to_result(operation: &str, errors: &[String]) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("could not {operation}: {}", errors.join("; "))
    }
}

fn validate_binary_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_PATH_TOKEN_BYTES
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.starts_with(".bitengine-")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        || Path::new(name).file_name().is_none()
    {
        bail!("unsafe binary filename: {name:?}");
    }
    Ok(())
}

fn validate_transaction_id(transaction_id: &str) -> Result<()> {
    if transaction_id.is_empty()
        || transaction_id.len() > MAX_PATH_TOKEN_BYTES
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
    let path = CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut statistics = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a live NUL-terminated string, `statistics` points to
    // writable storage for exactly one `statvfs` value, and the value is only
    // assumed initialized after libc reports success.
    if unsafe { libc::statvfs(path.as_ptr(), statistics.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the successful call above initialized every field.
    let statistics = unsafe { statistics.assume_init() };
    #[cfg(target_os = "macos")]
    let available_blocks = u64::from(statistics.f_bavail);
    #[cfg(not(target_os = "macos"))]
    let available_blocks = statistics.f_bavail;
    available_blocks.checked_mul(statistics.f_frsize)
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| ancestor.exists())
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

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open installation directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync installation directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, contents: &[u8]) -> Result<()> {
        let mut file = File::create(path)?;
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

        let installed = install_transaction_managed(
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
            &["bitcoind", "bitcoin-cli"],
            &destination,
            "test-success",
        )?;

        assert_eq!(installed, vec!["bitcoind", "bitcoin-cli"]);
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"new cli");
        assert_eq!(
            fs::metadata(destination.join("bitcoind"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(destination.join(INSTALL_LOCK_NAME))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!destination.join(INSTALL_JOURNAL_NAME).exists());
        Ok(())
    }

    #[test]
    fn first_install_durably_creates_a_nested_destination() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("node").join("Binaries");
        fs::create_dir(&source)?;
        write_file(&source.join("electrs"), b"new electrs")?;

        install_transaction_managed(
            &[InstallArtifact {
                source: source.join("electrs"),
                name: "electrs".to_owned(),
            }],
            &["electrs"],
            &destination,
            "first-install",
        )?;

        assert_eq!(fs::read(destination.join("electrs"))?, b"new electrs");
        assert!(fs::canonicalize(&destination)?.is_dir());
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

        let result = install_transaction_managed(
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
            &["bitcoind", "bitcoin-cli"],
            &destination,
            "test-failure",
        );

        assert!(result.is_err());
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
        Ok(())
    }

    #[test]
    fn mid_staging_source_change_cleans_prior_staged_files() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        write_file(&source.join("bitcoin-cli"), b"new cli")?;
        write_file(&destination.join("bitcoind"), b"old bitcoind")?;
        write_file(&destination.join("bitcoin-cli"), b"old cli")?;
        let artifacts = [
            InstallArtifact {
                source: source.join("bitcoind"),
                name: "bitcoind".to_owned(),
            },
            InstallArtifact {
                source: source.join("bitcoin-cli"),
                name: "bitcoin-cli".to_owned(),
            },
        ];
        let destination = fs::canonicalize(destination)?;
        let mut plans = build_plans(
            &artifacts,
            &["bitcoind", "bitcoin-cli"],
            &destination,
            "mid-stage-failure",
        )?;
        fs::remove_file(source.join("bitcoin-cli"))?;

        assert!(stage_all(&mut plans).is_err());
        cleanup_unjournaled(&plans)?;
        assert!(!plans[0].temporary.exists());
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
        Ok(())
    }

    #[test]
    fn managed_tombstones_remove_obsolete_binaries() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        write_file(&source.join("bitcoin-cli"), b"new cli")?;
        for name in ["bitcoind", "bitcoin-cli", "bitcoin-tx", "bitcoin-util"] {
            write_file(&destination.join(name), format!("old {name}").as_bytes())?;
        }

        install_transaction_managed(
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
            &["bitcoind", "bitcoin-cli", "bitcoin-tx", "bitcoin-util"],
            &destination,
            "test-tombstones",
        )?;

        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"new cli");
        assert!(!destination.join("bitcoin-tx").exists());
        assert!(!destination.join("bitcoin-util").exists());
        Ok(())
    }

    #[test]
    fn duplicate_artifact_and_managed_names_are_rejected() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        let artifacts = [
            InstallArtifact {
                source: source.join("bitcoind"),
                name: "bitcoind".to_owned(),
            },
            InstallArtifact {
                source: source.join("bitcoind"),
                name: "bitcoind".to_owned(),
            },
        ];

        assert!(install_transaction_managed(
            &artifacts,
            &["bitcoind"],
            &destination,
            "duplicate-artifact"
        )
        .is_err());
        assert!(install_transaction_managed(
            &artifacts[..1],
            &["bitcoind", "bitcoind"],
            &destination,
            "duplicate-managed"
        )
        .is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlink_destination_and_non_regular_entries_are_rejected() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        let real_destination = temporary.path().join("real-destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&real_destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        symlink(&real_destination, &destination)?;
        let artifact = InstallArtifact {
            source: source.join("bitcoind"),
            name: "bitcoind".to_owned(),
        };

        assert!(install_transaction_managed(
            std::slice::from_ref(&artifact),
            &["bitcoind"],
            &destination,
            "symlink-root"
        )
        .is_err());
        fs::remove_file(&destination)?;
        fs::create_dir(&destination)?;
        symlink(source.join("bitcoind"), destination.join("bitcoind"))?;
        assert!(install_transaction_managed(
            std::slice::from_ref(&artifact),
            &["bitcoind"],
            &destination,
            "symlink-entry"
        )
        .is_err());
        fs::remove_file(destination.join("bitcoind"))?;
        fs::create_dir(destination.join("bitcoind"))?;
        assert!(install_transaction_managed(
            std::slice::from_ref(&artifact),
            &["bitcoind"],
            &destination,
            "directory-entry"
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn destination_lock_rejects_a_second_installer() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let destination = temporary.path().join("destination");
        fs::create_dir(&destination)?;
        let destination = fs::canonicalize(destination)?;
        let _first = DestinationLock::acquire(&destination)?;
        assert!(DestinationLock::acquire(&destination).is_err());
        Ok(())
    }

    fn prepared_fixture(
        root: &Path,
        transaction_id: &str,
    ) -> Result<(PathBuf, Vec<InstallPlan>, InstallJournal)> {
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source)?;
        fs::create_dir_all(&destination)?;
        write_file(&source.join("bitcoind"), b"new bitcoind")?;
        write_file(&source.join("bitcoin-cli"), b"new cli")?;
        write_file(&destination.join("bitcoind"), b"old bitcoind")?;
        write_file(&destination.join("bitcoin-cli"), b"old cli")?;
        write_file(&destination.join("bitcoin-tx"), b"old tx")?;
        let artifacts = [
            InstallArtifact {
                source: source.join("bitcoind"),
                name: "bitcoind".to_owned(),
            },
            InstallArtifact {
                source: source.join("bitcoin-cli"),
                name: "bitcoin-cli".to_owned(),
            },
        ];
        let destination =
            prepare_destination(&destination, false)?.context("fixture destination")?;
        let mut plans = build_plans(
            &artifacts,
            &["bitcoind", "bitcoin-cli", "bitcoin-tx"],
            &destination,
            transaction_id,
        )?;
        stage_all(&mut plans)?;
        validate_prepared_inputs(&plans)?;
        let journal = InstallJournal::from_plans(transaction_id, &plans)?;
        write_journal(&destination, &journal, JournalWrite::Create)?;
        Ok((destination, plans, journal))
    }

    #[test]
    fn every_precommit_crash_boundary_restores_the_old_set() -> Result<()> {
        for boundary in 0..=6 {
            let temporary = tempfile::tempdir()?;
            let transaction_id = format!("crash-{boundary}");
            let (destination, plans, _journal) =
                prepared_fixture(temporary.path(), &transaction_id)?;
            let lock = DestinationLock::acquire(&destination)?;
            for (index, plan) in plans.iter().enumerate() {
                if index >= boundary {
                    break;
                }
                backup_one(plan)?;
            }
            if boundary > plans.len() {
                for plan in plans.iter().take(boundary - plans.len()) {
                    activate_one(plan)?;
                }
            }
            drop(lock);

            assert_eq!(
                recover_pending_install(&destination)?,
                InstallRecovery::RolledBack
            );
            assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
            assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
            assert_eq!(fs::read(destination.join("bitcoin-tx"))?, b"old tx");
            assert_eq!(
                recover_pending_install(&destination)?,
                InstallRecovery::None
            );
        }
        Ok(())
    }

    #[test]
    fn committed_crash_is_finalized_and_keeps_tombstones() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let (destination, plans, mut journal) =
            prepared_fixture(temporary.path(), "committed-crash")?;
        let lock = DestinationLock::acquire(&destination)?;
        backup_all(&plans)?;
        activate_all(&plans)?;
        sync_directory(&destination)?;
        journal.phase = JournalPhase::Committed;
        write_journal(&destination, &journal, JournalWrite::Replace)?;
        drop(lock);

        assert_eq!(
            recover_pending_install(&destination)?,
            InstallRecovery::Finalized
        );
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"new cli");
        assert!(!destination.join("bitcoin-tx").exists());
        Ok(())
    }

    #[test]
    fn partially_completed_rollback_is_finished_idempotently() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let (destination, plans, _journal) =
            prepared_fixture(temporary.path(), "partial-rollback")?;
        let lock = DestinationLock::acquire(&destination)?;
        backup_all(&plans)?;
        activate_all(&plans)?;
        remove_matching_file(
            &plans[0].destination,
            plans[0].staged.context("staged identity")?,
            "staged destination",
        )?;
        fs::rename(&plans[0].backup, &plans[0].destination)?;
        drop(lock);

        assert_eq!(
            recover_pending_install(&destination)?,
            InstallRecovery::RolledBack
        );
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
        assert_eq!(fs::read(destination.join("bitcoin-tx"))?, b"old tx");
        Ok(())
    }

    #[test]
    fn partially_completed_committed_cleanup_is_finished_idempotently() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let (destination, plans, mut journal) =
            prepared_fixture(temporary.path(), "partial-commit-cleanup")?;
        let lock = DestinationLock::acquire(&destination)?;
        backup_all(&plans)?;
        activate_all(&plans)?;
        sync_directory(&destination)?;
        journal.phase = JournalPhase::Committed;
        write_journal(&destination, &journal, JournalWrite::Replace)?;
        remove_matching_file(
            &plans[0].backup,
            plans[0].original.context("original identity")?,
            "transaction backup",
        )?;
        drop(lock);

        assert_eq!(
            recover_pending_install(&destination)?,
            InstallRecovery::Finalized
        );
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"new cli");
        assert!(!destination.join("bitcoin-tx").exists());
        Ok(())
    }

    #[test]
    fn temporary_prepared_journal_is_promoted_before_recovery() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let (destination, _plans, _journal) =
            prepared_fixture(temporary.path(), "temporary-journal")?;
        let lock = DestinationLock::acquire(&destination)?;
        fs::rename(
            destination.join(INSTALL_JOURNAL_NAME),
            destination.join(INSTALL_JOURNAL_TEMP_NAME),
        )?;
        drop(lock);

        assert_eq!(
            recover_pending_install(&destination)?,
            InstallRecovery::RolledBack
        );
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"old bitcoind");
        assert_eq!(fs::read(destination.join("bitcoin-cli"))?, b"old cli");
        assert_eq!(fs::read(destination.join("bitcoin-tx"))?, b"old tx");
        Ok(())
    }

    #[test]
    fn ambiguous_recovery_fails_closed_and_retains_journal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let (destination, plans, _journal) =
            prepared_fixture(temporary.path(), "ambiguous-recovery")?;
        let lock = DestinationLock::acquire(&destination)?;
        backup_one(&plans[0])?;
        activate_one(&plans[0])?;
        fs::remove_file(&plans[0].backup)?;
        drop(lock);

        assert!(recover_pending_install(&destination).is_err());
        assert!(destination.join(INSTALL_JOURNAL_NAME).is_file());
        assert_eq!(fs::read(destination.join("bitcoind"))?, b"new bitcoind");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_does_not_follow_a_symbolic_link_journal() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir()?;
        let destination = temporary.path().join("destination");
        let unrelated = temporary.path().join("unrelated.json");
        fs::create_dir(&destination)?;
        fs::write(&unrelated, b"sentinel")?;
        symlink(&unrelated, destination.join(INSTALL_JOURNAL_NAME))?;

        assert!(recover_pending_install(&destination).is_err());
        assert_eq!(fs::read(&unrelated)?, b"sentinel");
        assert!(
            fs::symlink_metadata(destination.join(INSTALL_JOURNAL_NAME))?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn final_journal_sync_failure_does_not_relabel_a_committed_set() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let journal = temporary.path().join(INSTALL_JOURNAL_NAME);
        fs::write(&journal, b"already validated")?;
        remove_internal_file_if_exists(&journal, "installation journal")?;

        finish_journal_removal(temporary.path(), &journal, |_| {
            Err(anyhow!("injected final directory sync failure"))
        })?;
        assert!(!journal.exists());
        Ok(())
    }

    #[test]
    fn available_space_uses_the_local_filesystem_directly() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        assert!(available_space_bytes(temporary.path()).is_some_and(|bytes| bytes > 0));
        Ok(())
    }
}
