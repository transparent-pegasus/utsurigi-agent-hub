use crate::config::no_symlinks;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

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
        private_dir(root)?;
        let path = root.join("operation.lock");
        no_symlinks(&path)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        private_file(&file)?;
        file.try_lock()
            .map_err(|_| Error::new("Another local operation is running"))?;
        Ok(Self { _file: file })
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
