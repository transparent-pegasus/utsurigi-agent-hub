use crate::config::{Config, no_symlinks};
use crate::error::{Error, IoContext, Result};
use crate::restic::hex_digest;
use crate::state::{Lock, private_dir, write_private};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Pointer {
    hub: String,
    current: String,
    previous: Option<String>,
}

impl Pointer {
    fn contains(&self, id: &str) -> bool {
        self.current == id || self.previous.as_deref() == Some(id)
    }
}

pub struct SharedCache {
    root: PathBuf,
    hub: String,
    observed: Option<Pointer>,
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
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::new(format!(
                "Cannot read shared CURRENT {}: {error}",
                path.display()
            )));
        }
    };
    let pointer: Pointer =
        serde_json::from_slice(&bytes).map_err(|_| Error::new("Invalid shared CURRENT pointer"))?;
    if pointer.hub != hub
        || !hex_digest(&pointer.current)
        || pointer
            .previous
            .as_ref()
            .is_some_and(|previous| !hex_digest(previous) || previous == &pointer.current)
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
        let _lock = Lock::cache_shared(&root)?;
        let observed = read_pointer(&root, &config.hub)?;
        Ok(Self {
            root,
            hub: config.hub.clone(),
            observed,
        })
    }

    fn version(&self, id: &str) -> PathBuf {
        self.root.join("versions").join(id)
    }

    fn cached(&self, pointer: &Option<Pointer>, id: &str) -> Result<Option<PathBuf>> {
        let version = self.version(id);
        no_symlinks(&version)?;
        if pointer.as_ref().is_some_and(|pointer| pointer.contains(id)) && version.is_dir() {
            return Ok(Some(version));
        }
        // Only an atomic rename of a fully restored and validated staging tree
        // creates a pending version. Unreferenced versions/ entries remain untrusted.
        let pending = self.root.join("pending").join(id);
        no_symlinks(&pending)?;
        Ok(pending.is_dir().then_some(pending))
    }

    pub fn load<T>(
        &self,
        id: &str,
        restore: impl FnOnce(&Path) -> Result<()>,
        validate: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        let lock = Lock::cache_shared(&self.root)?;
        let pointer = read_pointer(&self.root, &self.hub)?;
        if let Some(path) = self.cached(&pointer, id)? {
            return validate(&path);
        }
        // Release before taking an exclusive lock; never upgrade a live reader.
        drop(lock);
        let _lock = Lock::cache_exclusive(&self.root)?;
        let pointer = read_pointer(&self.root, &self.hub)?;
        // Another process may have completed this restore while we waited.
        if let Some(path) = self.cached(&pointer, id)? {
            return validate(&path);
        }
        private_dir(&self.root.join("versions"))?;
        let pending = self.root.join("pending");
        private_dir(&pending)?;
        let tree = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&pending)
            .at("Cannot stage shared import in", &pending)?;
        let staging = crate::selection::Export { tree, bytes: 0 };
        private_dir(staging.tree.path())?;
        restore(staging.tree.path())?;
        let loaded = validate(staging.tree.path())?;
        sync_tree(staging.tree.path())?;
        fs::rename(staging.tree.path(), pending.join(id))
            .at("Cannot stage shared version in", &pending)?;
        self.cleanup_pending(Some(id))?;
        File::open(&pending)
            .at("Cannot open shared path", &pending)?
            .sync_all()
            .at("Cannot sync shared path", &pending)?;
        // Callers own the parsed data, not paths or guards. Repository validation
        // and uploads can proceed without keeping any cache lock or tree alive.
        Ok(loaded)
    }

    fn cleanup_pending(&self, keep: Option<&str>) -> Result<()> {
        let pending = self.root.join("pending");
        for entry in fs::read_dir(&pending).at("Cannot read shared directory", &pending)? {
            let entry = entry.at("Cannot read shared cache entry", &pending)?;
            if keep.is_some_and(|id| entry.file_name() == id) {
                continue;
            }
            no_symlinks(&entry.path())?;
            crate::selection::remove_tree(&entry.path())?;
        }
        Ok(())
    }

    fn needs_publish(&self, pointer: &Option<Pointer>, id: &str) -> bool {
        pointer == &self.observed
            && pointer
                .as_ref()
                .is_none_or(|pointer| pointer.current != id || !self.version(id).is_dir())
    }

    pub fn publish(self, id: &str) -> Result<()> {
        let lock = Lock::cache_shared(&self.root)?;
        let pointer = read_pointer(&self.root, &self.hub)?;
        if !self.needs_publish(&pointer, id) {
            return Ok(());
        }
        drop(lock);
        let _lock = Lock::cache_exclusive(&self.root)?;
        let pointer = read_pointer(&self.root, &self.hub)?;
        // A slow upload must not roll back a newer Bot's publication. Compare
        // with the pointer observed before the repository snapshot query.
        if !self.needs_publish(&pointer, id) {
            return Ok(());
        }
        let version = self.version(id);
        let pending = self.root.join("pending");
        private_dir(&pending)?;
        let source = match self.cached(&pointer, id)? {
            Some(source) => source,
            None => {
                // A later load can supersede this owned metadata without moving CURRENT.
                for entry in fs::read_dir(&pending).at("Cannot read shared directory", &pending)? {
                    let entry = entry.at("Cannot read shared cache entry", &pending)?;
                    no_symlinks(&entry.path())?;
                    if entry.file_name().to_str().is_some_and(hex_digest) && entry.path().is_dir() {
                        return Ok(());
                    }
                }
                return Err(Error::new(
                    "Complete unpublished shared cache version is missing",
                ));
            }
        };
        if source != version {
            // An unreferenced version can remain after a crash before CURRENT was published.
            // Restore it afresh rather than trusting potentially incomplete local data.
            if version.exists() {
                crate::selection::remove_tree(&version)?;
            }
            fs::rename(&source, &version).at("Cannot publish shared version", &version)?;
            File::open(self.root.join("versions"))
                .at("Cannot open shared path", &self.root.join("versions"))?
                .sync_all()
                .at("Cannot sync shared path", &self.root.join("versions"))?;
            File::open(&pending)
                .at("Cannot open shared path", &pending)?
                .sync_all()
                .at("Cannot sync shared path", &pending)?;
        }
        // Repair a missing current directory without changing the pointer.
        if pointer
            .as_ref()
            .is_some_and(|pointer| pointer.current == id)
        {
            return Ok(());
        }
        let pointer = Pointer {
            hub: self.hub.clone(),
            current: id.to_owned(),
            previous: pointer.map(|pointer| pointer.current),
        };
        let bytes = serde_json::to_vec(&pointer)
            .map_err(|_| Error::new("Cannot encode shared CURRENT pointer"))?;
        write_private(&self.root.join("CURRENT"), &bytes)?;
        // The exclusive machine/hub lock stays held through every deletion. Readers
        // can hold a shared lock on operation.lock while using a version's files.
        for entry in fs::read_dir(self.root.join("versions"))
            .at("Cannot read shared directory", &self.root.join("versions"))?
        {
            let entry = entry.at(
                "Cannot read shared cache entry",
                &self.root.join("versions"),
            )?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == pointer.current || pointer.previous.as_deref() == Some(name) {
                continue;
            }
            if hex_digest(name) || name.starts_with(".staging-") {
                no_symlinks(&entry.path())?;
                crate::selection::remove_tree(&entry.path())?;
            }
        }
        // No staging tree is live outside an exclusive cache section. In-flight
        // operations hold owned metadata and skip publication if CURRENT changed.
        self.cleanup_pending(None)?;
        File::open(self.root.join("versions"))
            .at("Cannot open shared path", &self.root.join("versions"))?
            .sync_all()
            .at("Cannot sync shared path", &self.root.join("versions"))?;
        File::open(&pending)
            .at("Cannot open shared path", &pending)?
            .sync_all()
            .at("Cannot sync shared path", &pending)?;
        Ok(())
    }
}

fn sync_tree(path: &Path) -> Result<()> {
    no_symlinks(path)?;
    for entry in fs::read_dir(path).at("Cannot read shared directory", path)? {
        let entry = entry.at("Cannot read shared cache entry", path)?;
        let kind = entry
            .file_type()
            .at("Cannot inspect shared entry", &entry.path())?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())
                .at("Cannot open shared path", &entry.path())?
                .sync_all()
                .at("Cannot sync shared path", &entry.path())?;
        } else {
            return Err(Error::new(
                "Imported symlinks and special files are not allowed",
            ));
        }
    }
    File::open(path)
        .at("Cannot open shared path", path)?
        .sync_all()
        .at("Cannot sync shared path", path)?;
    Ok(())
}
