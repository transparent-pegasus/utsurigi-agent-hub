use crate::config::no_symlinks;
use crate::error::{Error, IoContext, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub hub: String,
    pub last_success: Option<i64>,
    pub anchor: Option<i64>,
    pub next_due: Option<i64>,
    #[serde(default)]
    pub joined: bool,
    #[serde(default)]
    pub failures: BTreeMap<String, Failure>,
    // Read legacy state, but derive the active cache exclusively from CURRENT.
    #[serde(default, rename = "active_shared", skip_serializing)]
    pub _active_shared: Option<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub operation: String,
    pub category: String,
    pub reason: String,
    pub next_action: String,
    pub recorded_at: i64,
}

impl Failure {
    pub fn sanitized(operation: &str, error: &Error) -> Self {
        // Match only known categories. Never persist the child diagnostic,
        // repository paths, environment, or any part of a credential value.
        let message = error
            .message
            .split("; stderr:")
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let (category, reason, next_action) = if error.code == 12
            || message.contains("password")
            || message.contains("credential")
            || message.contains("authentication")
        {
            (
                "authentication",
                "Repository authentication failed or a required password was not delivered",
                "Check host-delivered repository credentials, then retry this operation",
            )
        } else if error.code == 11 || message.contains("lock") {
            (
                "locked",
                "A required repository or local state lock is unavailable",
                "Let the active operation finish, then retry; preserve existing locks",
            )
        } else if message.contains("budget") || message.contains("attestation") {
            (
                "budget",
                "The source is outside the authorized storage budget",
                "Provide a valid repository and source-size budget attestation, then retry",
            )
        } else if message.contains("max_bots") || message.contains("fleet") {
            (
                "capacity",
                "The shared max_bots cap cannot admit this registration",
                "Review the current shared registry capacity, then retry setup",
            )
        } else if message.contains("private")
            || message.contains("history")
            || message.contains("master key")
        {
            (
                "private_history",
                "Repository identity or private-history safeguards rejected the operation",
                "Review shared identifier history and use a separate private repository, password access and master key",
            )
        } else if message.contains("runtime") || message.contains("superseded") {
            (
                "identity",
                "The Bot runtime or repository conflicts with the current shared identity",
                "Reconcile the Bot runtime and repository with the current registry; local work was preserved",
            )
        } else if message.contains("stale")
            || message.contains("authoring")
            || message.contains("revision")
            || message.contains("conflict")
        {
            (
                "conflict",
                "The authoring base is missing, stale, or lost a shared-head comparison",
                "Copy verified active_shared, omit generated registry-history.json, reconcile local edits, then explicitly publish",
            )
        } else if message.contains("decision") {
            (
                "decisions",
                "The decision record has invalid schema, provenance, or scope",
                "Correct decisions.yaml for this hub and shared repository, then retry",
            )
        } else if message.contains("restic")
            && (message.contains("unavailable")
                || message.contains("incompatible")
                || message.contains("executable"))
        {
            (
                "restic",
                "A compatible restic executable could not be opened",
                "Configure or install the pinned restic executable, then retry setup",
            )
        } else if error.code == 10 {
            (
                "repository",
                "The required repository does not exist",
                "Check the configured repository location and run setup or explicit recovery",
            )
        } else if message.contains("snapshot")
            || message.contains("fence")
            || message.contains("registry")
        {
            (
                "shared_data",
                "Shared snapshot, registry or migration metadata could not be verified",
                "Inspect the configured shared repository and recover valid data before retrying",
            )
        } else {
            (
                "operation",
                "The operation could not complete with the current configuration or storage",
                "Check local configuration, source files and storage availability, then retry this operation",
            )
        };
        Self {
            operation: operation.to_owned(),
            category: category.to_owned(),
            reason: reason.to_owned(),
            next_action: next_action.to_owned(),
            recorded_at: chrono::Utc::now().timestamp(),
        }
    }
}

pub struct Lock {
    _file: File,
}

pub fn private_dir(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .at("Cannot create private directory", path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .at("Cannot protect directory", path)?;
    }
    Ok(())
}

pub fn private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .at("Cannot protect file", path)?;
    }
    Ok(())
}

impl Lock {
    pub fn acquire(root: &Path) -> Result<Self> {
        Self::acquire_with_timeout(root, false, "Bot state exclusive", Duration::ZERO)
    }

    pub fn cache_shared(root: &Path) -> Result<Self> {
        Self::acquire_with_timeout(root, true, "shared cache shared", Duration::from_secs(30))
    }

    pub fn cache_exclusive(root: &Path) -> Result<Self> {
        Self::acquire_with_timeout(
            root,
            false,
            "shared cache exclusive",
            Duration::from_secs(30),
        )
    }

    fn acquire_with_timeout(
        root: &Path,
        shared: bool,
        kind: &str,
        timeout: Duration,
    ) -> Result<Self> {
        let path = root.join("operation.lock");
        private_dir(root).map_err(|error| {
            Error::new(format!(
                "Cannot prepare {kind} lock at {}: {}",
                path.display(),
                error.message
            ))
        })?;
        no_symlinks(&path).map_err(|error| {
            Error::new(format!(
                "Cannot open {kind} lock at {}: {}",
                path.display(),
                error.message
            ))
        })?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                Error::new(format!(
                    "Cannot open {kind} lock at {}: {error}",
                    path.display()
                ))
            })?;
        private_file(&file, &path).map_err(|error| {
            Error::new(format!(
                "Cannot protect {kind} lock at {}: {}",
                path.display(),
                error.message
            ))
        })?;
        let started = Instant::now();
        loop {
            let result = if shared {
                file.try_lock_shared()
            } else {
                file.try_lock()
            };
            match result {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if timeout.is_zero() {
                        return Err(Error::new(format!(
                            "{kind} lock at {} is busy; another local operation is running",
                            path.display()
                        )));
                    }
                    let remaining = timeout.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::new(format!(
                            "Timed out waiting for {kind} lock at {} after {timeout:?}",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(25).min(remaining));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(Error::new(format!(
                        "Cannot acquire {kind} lock at {}: {error}",
                        path.display()
                    )));
                }
            }
        }
    }
}

impl State {
    pub fn load(root: &Path, hub: &str) -> Result<Self> {
        let path = root.join("status.json");
        no_symlinks(&path)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::new("Invalid persisted state"))?;
                if state.hub != hub {
                    return Err(Error::new(
                        "Persisted state hub does not match configuration",
                    ));
                }
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                hub: hub.to_owned(),
                ..Self::default()
            }),
            Err(e) => Err(Error::new(format!(
                "Cannot read state {}: {e}",
                path.display()
            ))),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|_| Error::new("Cannot encode state"))?;
        write_private(&root.join("status.json"), &bytes)
    }
}

/// Publish a private local record only after its contents have reached disk.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let root = path
        .parent()
        .ok_or_else(|| Error::new("Record requires a parent directory"))?;
    let mut temp = tempfile::NamedTempFile::new_in(root).at("Cannot stage record in", root)?;
    private_file(temp.as_file(), path)?;
    temp.write_all(bytes).at("Cannot write record", path)?;
    temp.flush().at("Cannot flush record", path)?;
    temp.as_file().sync_all().at("Cannot sync record", path)?;
    no_symlinks(path)?;
    temp.persist(path).map_err(|error| {
        Error::new(format!(
            "Cannot publish record {}: {}",
            path.display(),
            error.error
        ))
    })?;
    File::open(root)
        .at("Cannot open directory", root)?
        .sync_all()
        .at("Cannot sync directory", root)
}

pub fn pending_backup(root: &Path) -> PathBuf {
    root.join("pending-backup.yaml")
}

pub fn require_reconciled(root: &Path) -> Result<()> {
    let path = pending_backup(root);
    no_symlinks(&path)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::new(format!(
            "Cannot inspect {}: {error}",
            path.display()
        ))),
        Ok(_) => Err(Error::new(format!(
            "Previous backup needs reconciliation; inspect snapshots using {}, reconcile the recorded run, then remove this file before another write",
            path.display()
        ))),
    }
}

pub fn begin_backup(
    config: &crate::config::Config,
    repository: &crate::config::Repository,
    export: &crate::selection::Export,
    kind: &str,
) -> Result<()> {
    let root = config.paths.state.as_ref().expect("runtime configuration");
    require_reconciled(root)?;
    let record = serde_json::json!({
        "version": 1, "hub": config.hub, "bot": config.id(),
        "runtime": config.runtime_id(), "repository": repository.url,
        "kind": kind, "run": export.run_id(),
    });
    // All fields are scalars. JSON quoting is valid YAML and preserves unusual paths.
    let yaml: String = record
        .as_object()
        .expect("record mapping")
        .iter()
        .map(|(key, value)| format!("{key}: {value}\n"))
        .collect();
    write_private(&pending_backup(root), yaml.as_bytes())
}

pub fn finish_backup(root: &Path) -> Result<()> {
    let path = pending_backup(root);
    fs::remove_file(&path).at("Cannot clear backup record", &path)?;
    File::open(root)
        .at("Cannot open directory", root)?
        .sync_all()
        .at("Cannot sync directory", root)
}

pub fn fresh_destination(destination: &Path, staging: tempfile::TempDir) -> Result<()> {
    no_symlinks(destination)?;
    // Reserve the destination exclusively before moving the validated tree.
    fs::create_dir(destination).map_err(|error| {
        Error::new(format!(
            "Restore destination must be a new directory with an existing parent: {}: {error}",
            destination.display()
        ))
    })?;
    let result = (|| {
        private_dir(destination)?;
        private_dir(staging.path())?;
        fs::rename(staging.path(), destination).at("Cannot publish restored tree in", destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir(destination);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_state_contention_names_the_lock_and_path() {
        let root = tempfile::tempdir().unwrap();
        let _held = Lock::acquire(root.path()).unwrap_or_else(|error| panic!("{}", error.message));
        let error = Lock::acquire(root.path())
            .err()
            .expect("state must serialize");
        assert!(error.message.contains("Bot state"), "{}", error.message);
        assert!(
            error
                .message
                .contains(&root.path().join("operation.lock").display().to_string())
        );
    }

    #[test]
    fn lock_io_failure_is_named_and_not_reported_as_contention() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("operation.lock")).unwrap();
        let error = Lock::acquire(root.path())
            .err()
            .expect("cannot lock a directory");
        assert!(error.message.contains("Bot state"), "{}", error.message);
        assert!(
            error.message.contains("operation.lock"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("running"), "{}", error.message);
        assert!(!error.message.contains("Timed out"), "{}", error.message);
    }

    #[test]
    fn cache_lock_timeout_is_bounded_and_names_mode_and_path() {
        for (shared, kind) in [
            (true, "shared cache shared"),
            (false, "shared cache exclusive"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let _held =
                Lock::acquire(root.path()).unwrap_or_else(|error| panic!("{}", error.message));
            let started = Instant::now();
            let timeout = Duration::from_millis(40);
            let error = Lock::acquire_with_timeout(root.path(), shared, kind, timeout)
                .err()
                .expect("held exclusive lock must time out");
            assert!(started.elapsed() >= timeout);
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(error.message.contains("Timed out"), "{}", error.message);
            assert!(error.message.contains(kind), "{}", error.message);
            assert!(
                error
                    .message
                    .contains(root.path().join("operation.lock").to_str().unwrap())
            );
        }
    }
}
