use crate::config::{Config, no_symlinks};
use crate::error::{Error, Result};
use crate::restic::snapshot_id;
use crate::state::{Lock, private_dir, private_file};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Pointer {
    hub: String,
    current: String,
    previous: Option<String>,
}

pub struct SharedCache {
    root: PathBuf,
    hub: String,
    pointer: Option<Pointer>,
    _lock: Lock,
}

fn root(config: &Config) -> Option<PathBuf> {
    // The host supplies one machine-wide imports root; machine.root is metadata only.
    config
        .paths
        .imports
        .as_ref()
        .map(|imports| imports.join(format!("{:x}", Sha256::digest(config.hub.as_bytes()))))
}

fn read_pointer(root: &Path, hub: &str) -> Result<Option<Pointer>> {
    let path = root.join("CURRENT");
    no_symlinks(&path)?;
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let pointer: Pointer =
        serde_json::from_slice(&bytes).map_err(|_| Error::new("Invalid shared CURRENT pointer"))?;
    if pointer.hub != hub
        || !snapshot_id(&pointer.current)
        || pointer
            .previous
            .as_ref()
            .is_some_and(|previous| !snapshot_id(previous) || previous == &pointer.current)
    {
        return Err(Error::new("Invalid shared CURRENT pointer"));
    }
    for id in std::iter::once(&pointer.current).chain(&pointer.previous) {
        no_symlinks(&root.join("versions").join(id))?;
    }
    Ok(Some(pointer))
}

pub fn active(config: &Config) -> Result<Option<PathBuf>> {
    let Some(root) = root(config) else {
        return Ok(None);
    };
    let Some(pointer) = read_pointer(&root, &config.hub)? else {
        return Ok(None);
    };
    let path = root.join("versions").join(pointer.current);
    Ok(path.is_dir().then_some(path))
}

impl SharedCache {
    pub fn open(config: &Config) -> Result<Self> {
        let root = root(config).expect("runtime configuration");
        let lock = Lock::acquire(&root)?;
        let pointer = read_pointer(&root, &config.hub)?;
        private_dir(&root.join("versions"))?;
        Ok(Self {
            root,
            hub: config.hub.clone(),
            pointer,
            _lock: lock,
        })
    }

    pub fn version(&self, id: &str) -> PathBuf {
        self.root.join("versions").join(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.pointer
            .as_ref()
            .is_some_and(|pointer| pointer.current == id || pointer.previous.as_deref() == Some(id))
            && self.version(id).is_dir()
    }

    pub fn staging(&self) -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(self.root.join("versions"))?)
    }

    pub fn publish(self, id: &str, staging: Option<tempfile::TempDir>) -> Result<PathBuf> {
        let version = self.version(id);
        no_symlinks(&version)?;
        if let Some(staging) = staging {
            sync_tree(staging.path())?;
            // An unreferenced version can remain after a crash before CURRENT was published.
            // Restore it afresh rather than trusting potentially incomplete local data.
            if version.exists() {
                fs::remove_dir_all(&version)?;
            }
            fs::rename(staging.path(), &version)?;
            File::open(self.root.join("versions"))?.sync_all()?;
        }
        let pointer = if let Some(pointer) = &self.pointer
            && pointer.current == id
        {
            pointer
        } else {
            &Pointer {
                hub: self.hub.clone(),
                current: id.to_owned(),
                previous: self.pointer.as_ref().map(|pointer| pointer.current.clone()),
            }
        };
        if self.pointer.as_ref().is_none_or(|old| old.current != id) {
            let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
            private_file(temp.as_file())?;
            serde_json::to_writer(&mut temp, pointer)
                .map_err(|_| Error::new("Cannot encode shared CURRENT pointer"))?;
            temp.flush()?;
            temp.as_file().sync_all()?;
            no_symlinks(&self.root.join("CURRENT"))?;
            temp.persist(self.root.join("CURRENT"))
                .map_err(|_| Error::new("Cannot publish shared CURRENT pointer"))?;
            File::open(&self.root)?.sync_all()?;
        }
        // The exclusive machine/hub lock stays held through every deletion. Readers
        // can hold a shared lock on operation.lock while using a version's files.
        for entry in fs::read_dir(self.root.join("versions"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == pointer.current || pointer.previous.as_deref() == Some(name) {
                continue;
            }
            if snapshot_id(name) || name.starts_with(".staging-") {
                no_symlinks(&entry.path())?;
                fs::remove_dir_all(entry.path())?;
            }
        }
        File::open(self.root.join("versions"))?.sync_all()?;
        Ok(version)
    }
}

fn sync_tree(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())?.sync_all()?;
        } else {
            return Err(Error::new(
                "Imported symlinks and special files are not allowed",
            ));
        }
    }
    File::open(path)?.sync_all()?;
    Ok(())
}
