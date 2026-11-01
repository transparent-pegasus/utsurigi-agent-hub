use crate::config::no_symlinks;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub hub: String,
    pub last_success: Option<i64>,
    pub anchor: Option<i64>,
    pub next_due: Option<i64>,
    #[serde(default, rename = "active_shared", skip_serializing)]
    pub _active_shared: Option<PathBuf>,
}

pub struct Lock {
    _file: File,
}

pub fn private_dir(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
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
        private_file(&file).map_err(|error| {
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
        match fs::read(path) {
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
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        private_file(temp.as_file())?;
        serde_json::to_writer(&mut temp, self).map_err(|_| Error::new("Cannot encode state"))?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        no_symlinks(&root.join("status.json"))?;
        temp.persist(root.join("status.json"))
            .map_err(|_| Error::new("Cannot publish state"))?;
        File::open(root)?.sync_all()?;
        Ok(())
    }
}

pub fn fresh_destination(destination: &Path, staging: tempfile::TempDir) -> Result<()> {
    no_symlinks(destination)?;
    // Reserve the destination exclusively before moving the validated tree.
    fs::create_dir(destination).map_err(|_| {
        Error::new("Restore destination must be a new directory with an existing parent")
    })?;
    let result = (|| {
        private_dir(destination)?;
        for entry in fs::read_dir(staging.path())? {
            let entry = entry?;
            fs::rename(entry.path(), destination.join(entry.file_name()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
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
