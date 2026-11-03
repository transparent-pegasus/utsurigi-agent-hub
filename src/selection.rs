use crate::config::{Config, no_symlinks, relative};
use crate::error::{Error, Result};
use crate::state::private_dir;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub struct Export {
    pub tree: tempfile::TempDir,
    pub bytes: u64,
}

fn excluded(path: &Path, extra: &GlobSet) -> bool {
    if path.ancestors().any(|ancestor| extra.is_match(ancestor)) {
        return true;
    }
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git"
                | ".ssh"
                | ".gnupg"
                | ".aws"
                | ".azure"
                | ".kube"
                | ".docker"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | "gcloud"
                | ".secrets"
                | "secrets"
                | "private_keys"
                | "credentials"
                | "auth"
                | "authentication"
                | "auth_state"
                | "auth-state"
                | "sessions"
                | "cookies"
                | ".cache"
                | "__pycache__"
                | "node_modules"
                | "models"
                | ".models"
                | "model_weights"
                | "weights"
                | "index"
                | "indexes"
                | ".indexes"
                | "generated_index"
                | "generated_indexes"
                | "generated-index"
                | "index_cache"
                | "hnsw_index"
                | "indices"
                | "vector_index"
                | "vector-index"
                | "chroma"
                | ".chroma"
                | ".lancedb"
                | "logs"
        ) || name == ".env"
            || name.starts_with(".env.")
            || name.ends_with(".env")
            || name.starts_with("credentials.")
            || name.starts_with("secrets.")
            || name.starts_with(".secrets.")
            || name.starts_with("auth.")
            || name.starts_with("auth_state.")
            || name.starts_with("auth-state.")
            || name.starts_with("token.")
            || name.starts_with("tokens.")
            || name.starts_with("cookies.")
            || name.starts_with("logs.")
            || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
                .iter()
                .any(|v| name.starts_with(v))
            || [
                ".pem",
                ".key",
                ".p12",
                ".pfx",
                ".keystore",
                ".gguf",
                ".ggml",
                ".safetensors",
                ".onnx",
                ".pt",
                ".pth",
                ".ckpt",
                ".faiss",
                ".index",
                ".log",
                "-wal",
                "-shm",
                ".wal",
                ".shm",
            ]
            .iter()
            .any(|v| name.ends_with(v))
            || name.contains(".log.")
            || (name.starts_with("pytorch_model") && name.ends_with(".bin"))
            || matches!(name.as_str(), "model.bin" | "weights.bin")
    })
}

fn exclusions(config: &Config) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in &config.data.exclude {
        builder.add(Glob::new(pattern).map_err(|_| Error::new("Invalid exclusion pattern"))?);
        if !pattern.contains('/') {
            builder.add(
                Glob::new(&format!("**/{pattern}"))
                    .map_err(|_| Error::new("Invalid exclusion pattern"))?,
            );
        }
    }
    builder
        .build()
        .map_err(|_| Error::new("Invalid exclusion patterns"))
}

pub fn export(config: &Config, shared: bool) -> Result<Export> {
    let root = if shared {
        &config.paths.shared
    } else {
        &config.paths.owned
    };
    let root = root.as_ref().expect("runtime configuration");
    let state = config.paths.state.as_ref().expect("runtime configuration");
    no_symlinks(root)?;
    if !root.is_dir() {
        return Err(Error::new("Source root must be a directory"));
    }
    let tree = tempfile::Builder::new()
        .prefix("export-")
        .tempdir_in(state)?;
    let extra = if shared {
        GlobSetBuilder::new()
            .build()
            .map_err(|_| Error::new("Invalid exclusions"))?
    } else {
        exclusions(config)?
    };
    let mut bytes = 0;
    let includes = if shared {
        vec![std::path::PathBuf::from(".")]
    } else {
        config.data.include.clone()
    };
    for include in includes {
        if !shared && excluded(&include, &extra) {
            return Err(Error::new(format!(
                "Explicit data.include path is excluded: {include:?}"
            )));
        }
        copy_tree(
            root,
            &include,
            tree.path(),
            &extra,
            config,
            shared,
            &mut bytes,
        )?;
    }
    Ok(Export { tree, bytes })
}

fn copy_tree(
    root: &Path,
    relative_path: &Path,
    destination: &Path,
    extra: &GlobSet,
    config: &Config,
    shared: bool,
    bytes: &mut u64,
) -> Result<()> {
    if !relative(relative_path) {
        return Err(Error::new("Invalid selected path"));
    }
    if excluded(relative_path, extra) {
        eprintln!(
            "Excluded {} path: {relative_path:?}",
            if shared { "shared" } else { "owned" }
        );
        return Ok(());
    }
    let source = root.join(relative_path);
    no_symlinks(&source)?;
    let metadata = fs::symlink_metadata(&source)?;
    let target = destination.join(relative_path);
    if metadata.is_dir() {
        private_dir(&target)?;
        let mut entries: Vec<_> = fs::read_dir(&source)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_tree(
                root,
                &relative_path.join(entry.file_name()),
                destination,
                extra,
                config,
                shared,
                bytes,
            )?;
        }
    } else if metadata.is_file() {
        if target.exists() {
            return Ok(());
        }
        let mut file = open_source(root, relative_path)?;
        let before = file.metadata()?;
        if !before.is_file() {
            return Err(Error::new(
                "Only regular files and directories can be selected",
            ));
        }
        let mut prefix = [0u8; 16];
        let count = file.read(&mut prefix)?;
        if count == 16 && prefix == *b"SQLite format 3\0" {
            if shared
                || !config.data.sqlite_snapshots.iter().any(|p| {
                    p.components()
                        .filter(|c| *c != std::path::Component::CurDir)
                        .eq(relative_path
                            .components()
                            .filter(|c| *c != std::path::Component::CurDir))
                })
            {
                return Err(Error::new(
                    "SQLite data requires a declared consistent host snapshot",
                ));
            }
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = source.as_os_str().to_os_string();
                sidecar.push(suffix);
                match fs::symlink_metadata(sidecar) {
                    Ok(_) => {
                        return Err(Error::new(
                            "SQLite snapshot has WAL/SHM sidecars; stop writers and export consistently",
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if let Some(parent) = target.parent() {
            private_dir(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        output.write_all(&prefix[..count])?;
        let copied = std::io::copy(&mut file, &mut output)? + count as u64;
        let after = file.metadata()?;
        if before.len() != copied
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            return Err(Error::new(
                "Selected file changed during export; keep source snapshots quiescent",
            ));
        }
        *bytes = bytes
            .checked_add(copied)
            .ok_or_else(|| Error::new("Source size overflow"))?;
    } else {
        return Err(Error::new(
            "Selected symlinks and special files are not allowed",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_source(root: &Path, relative: &Path) -> Result<File> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    let mut parts = root
        .components()
        .chain(relative.components())
        .filter_map(|part| {
            if let Component::Normal(name) = part {
                Some(name)
            } else {
                None
            }
        })
        .peekable();
    let mut file = File::open("/")?;
    // Resolve every component without following concurrent symlink replacements.
    while let Some(part) = parts.next() {
        let name = CString::new(part.as_bytes()).map_err(|_| Error::new("Invalid source path"))?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if parts.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(Error::new(
                "Cannot open selected source without following symlinks",
            ));
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

#[cfg(not(unix))]
pub(crate) fn open_source(_root: &Path, _relative: &Path) -> Result<File> {
    Err(Error::new(
        "Secure source export requires a supported Unix host",
    ))
}

pub fn validate_import(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            validate_import(&entry.path())?;
        } else if !kind.is_file() {
            return Err(Error::new(
                "Imported symlinks and special files are not allowed",
            ));
        }
    }
    Ok(())
}
