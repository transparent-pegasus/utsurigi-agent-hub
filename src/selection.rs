use crate::config::{Config, no_symlinks, relative};
use crate::error::{Error, IoContext, Result};
use crate::state::private_dir;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Export {
    pub tree: tempfile::TempDir,
    pub bytes: u64,
}

impl Drop for Export {
    fn drop(&mut self) {
        // TempDir removes children after this; preserved directories may be read-only.
        let _ = writable_directories(self.tree.path(), Path::new("."));
    }
}

impl Export {
    pub fn run_id(&self) -> String {
        self.tree
            .path()
            .file_name()
            .expect("temporary directory")
            .to_string_lossy()
            .into_owned()
    }
    pub fn remove(&mut self, name: &str) -> Result<()> {
        let path = self.tree.path().join(name);
        no_symlinks(&path)?;
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                fs::remove_file(path)?;
                self.bytes = self
                    .bytes
                    .checked_sub(metadata.len())
                    .ok_or_else(|| Error::new("Invalid export size"))?;
            }
            Ok(_) => return Err(Error::new("Shared metadata must be a regular file")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub fn write(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.remove(name)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.tree.path().join(name))?;
        crate::state::private_file(&file, &self.tree.path().join(name))?;
        file.write_all(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::new("Source size overflow"))?;
        Ok(())
    }
}

// Accepted remote and retained migration trees are already selected content.
// Copy every byte and empty directory, without applying today's author filters.
pub fn copy_verified(config: &Config, root: &Path) -> Result<Export> {
    fn copy(root: &Path, relative: &Path, target: &Path, bytes: &mut u64) -> Result<()> {
        no_symlinks(&root.join(relative))?;
        private_dir(&target.join(relative))?;
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let name = relative.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                copy(root, &name, target, bytes)?;
            } else if kind.is_file() {
                let mut source = open_source(root, &name)?;
                let before = source.metadata()?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target.join(&name))?;
                crate::state::private_file(&output, &target.join(&name))?;
                let size = std::io::copy(&mut source, &mut output)?;
                let after = source.metadata()?;
                if !before.is_file() || before.len() != size || !unchanged(&before, &after)? {
                    return Err(Error::new(
                        "Shared source changed during copy; retry the read",
                    ));
                }
                preserve_metadata(&output, &before)?;
                *bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| Error::new("Source size overflow"))?;
            } else {
                return Err(Error::new(
                    "Imported symlinks and special files are not allowed",
                ));
            }
        }
        Ok(())
    }
    no_symlinks(root)?;
    let tree = tempfile::Builder::new()
        .prefix("shared-copy-")
        .tempdir_in(config.paths.state.as_ref().expect("runtime configuration"))?;
    private_dir(tree.path())?;
    let mut bytes = 0;
    copy(root, Path::new("."), tree.path(), &mut bytes)?;
    let export = Export { tree, bytes };
    preserve_directories(root, export.tree.path(), Path::new("."))?;
    Ok(export)
}

fn preserve_metadata(file: &File, metadata: &Metadata) -> Result<()> {
    file.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
    file.set_permissions(metadata.permissions())?;
    Ok(())
}

fn writable_directories(root: &Path, relative: &Path) -> Result<()> {
    let directory = open_source(root, relative)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() {
        return Err(Error::new(
            "Cleanup requires an application-owned directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        directory.set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() | 0o700,
        ))?;
    }
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            writable_directories(root, &relative.join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub(crate) fn remove_tree(root: &Path) -> Result<()> {
    no_symlinks(root)?;
    writable_directories(root, Path::new("."))?;
    fs::remove_dir_all(root).at("Cannot remove application-owned tree", root)
}

fn unchanged(before: &Metadata, after: &Metadata) -> Result<bool> {
    let same = before.len() == after.len()
        && before.modified()? == after.modified()?
        && before.permissions() == after.permissions();
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt;
        same && before.ctime() == after.ctime() && before.ctime_nsec() == after.ctime_nsec()
    };
    Ok(same)
}

fn preserve_directories(root: &Path, target: &Path, relative: &Path) -> Result<()> {
    for entry in fs::read_dir(target.join(relative))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            preserve_directories(root, target, &relative.join(entry.file_name()))?;
        }
    }
    let source = open_source(root, relative)?;
    let metadata = source.metadata()?;
    if !metadata.is_dir() {
        return Err(Error::new("Selected directory changed during export"));
    }
    let directory = File::open(target.join(relative))?;
    // Apply after all includes/children: creating files changes parent mtimes.
    directory.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
    if relative != Path::new(".") {
        directory.set_permissions(metadata.permissions())?;
    }
    Ok(())
}

fn link_captured(root: &Path, relative: &Path, target: &Path, source: &File) -> Result<()> {
    no_symlinks(&root.join(relative))?;
    fs::hard_link(root.join(relative), target.join(relative))?;
    let linked = open_source(target, relative)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let before = source.metadata()?;
        let after = linked.metadata()?;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(Error::new("Captured export changed during reuse"));
        }
    }
    Ok(())
}

fn reuse_captured(
    config: &Config,
    relative: &Path,
    target: &Path,
    source: &mut File,
) -> Result<bool> {
    let root = config
        .paths
        .state
        .as_ref()
        .expect("runtime configuration")
        .join("owned-export");
    let path = root.join(relative);
    // A previous capture may have files where the current source has directories.
    let mut cursor = std::path::PathBuf::new();
    for component in path.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::new(format!(
                    "Symlink paths are not allowed: {}",
                    cursor.display()
                )));
            }
            Ok(_) => (),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => {
                return Err(Error::new(format!(
                    "Cannot inspect path {}: {error}",
                    cursor.display()
                )));
            }
        }
    }
    let mut captured = open_source(&root, relative)?;
    let before = captured.metadata()?;
    let original = source.metadata()?;
    if !before.is_file()
        || before.len() != original.len()
        || before.modified()? != original.modified()?
        || before.permissions() != original.permissions()
    {
        return Ok(false);
    }
    source.rewind()?;
    let mut current = [0; 64 * 1024];
    let mut previous = [0; 64 * 1024];
    loop {
        let count = source.read(&mut current)?;
        if count == 0 {
            break;
        }
        captured.read_exact(&mut previous[..count])?;
        if current[..count] != previous[..count] {
            return Ok(false);
        }
    }
    if !unchanged(&before, &captured.metadata()?)? {
        return Ok(false);
    }
    link_captured(&root, relative, target, &captured)?;
    Ok(true)
}

fn retain_owned(config: &Config, export: &Path) -> Result<()> {
    fn link_tree(root: &Path, relative: &Path, target: &Path) -> Result<()> {
        private_dir(&target.join(relative))?;
        for entry in fs::read_dir(root.join(relative))? {
            let entry = entry?;
            let name = relative.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                link_tree(root, &name, target)?;
            } else {
                let source = open_source(root, &name)?;
                if !source.metadata()?.is_file() {
                    return Err(Error::new("Captured export must contain regular files"));
                }
                link_captured(root, &name, target, &source)?;
            }
        }
        Ok(())
    }
    let state = config.paths.state.as_ref().expect("runtime configuration");
    let tree = tempfile::Builder::new()
        .prefix("owned-capture-")
        .tempdir_in(state)?;
    let capture = Export { tree, bytes: 0 };
    link_tree(export, Path::new("."), capture.tree.path())?;
    preserve_directories(export, capture.tree.path(), Path::new("."))?;
    let previous = state.join("owned-export");
    no_symlinks(&previous)?;
    if previous.try_exists()? {
        remove_tree(&previous)?;
    }
    fs::rename(capture.tree.path(), &previous)?;
    Ok(())
}

const EXCLUDED_NAMES: &[&str] = &[
    ".git",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "gcloud",
    ".secrets",
    "secrets",
    "private_keys",
    "credentials",
    "auth",
    "authentication",
    "auth_state",
    "auth-state",
    "sessions",
    "cookies",
    ".cache",
    "__pycache__",
    "node_modules",
    "models",
    ".models",
    "model_weights",
    "weights",
    "index",
    "indexes",
    ".indexes",
    "generated_index",
    "generated_indexes",
    "generated-index",
    "index_cache",
    "hnsw_index",
    "indices",
    "vector_index",
    "vector-index",
    "chroma",
    ".chroma",
    ".lancedb",
    "logs",
    "model.bin",
    "weights.bin",
];

const EXCLUDED_PREFIXES: &[&str] = &[
    ".env.",
    "credentials.",
    "secrets.",
    ".secrets.",
    "auth.",
    "auth_state.",
    "auth-state.",
    "token.",
    "tokens.",
    "cookies.",
    "logs.",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

const EXCLUDED_SUFFIXES: &[&str] = &[
    ".env",
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
];

fn excluded(path: &Path, extra: &GlobSet) -> bool {
    if path.ancestors().any(|ancestor| extra.is_match(ancestor)) {
        return true;
    }
    path.components().any(|part| {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        EXCLUDED_NAMES.contains(&name.as_str())
            || EXCLUDED_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            || EXCLUDED_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix))
            || name.contains(".log.")
            || (name.starts_with("pytorch_model") && name.ends_with(".bin"))
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
        return Err(Error::new(format!(
            "Source root must be a directory: {}",
            root.display()
        )));
    }
    let tree = tempfile::Builder::new()
        .prefix("export-")
        .tempdir_in(state)
        .at("Cannot create export staging in", state)?;
    private_dir(tree.path())?;
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
    let export = Export { tree, bytes };
    preserve_directories(root, export.tree.path(), Path::new("."))?;
    if !shared {
        retain_owned(config, export.tree.path())?;
    }
    Ok(export)
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
    let metadata = fs::symlink_metadata(&source).at("Cannot inspect source", &source)?;
    let target = destination.join(relative_path);
    if metadata.is_dir() {
        private_dir(&target)?;
        let mut entries: Vec<_> = fs::read_dir(&source)
            .at("Cannot read source directory", &source)?
            .collect::<std::io::Result<_>>()
            .at("Cannot read source entry", &source)?;
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
        let before = file.metadata().at("Cannot inspect source", &source)?;
        if !before.is_file() {
            return Err(Error::new(
                "Only regular files and directories can be selected",
            ));
        }
        let mut prefix = [0u8; 16];
        let count = file.read(&mut prefix).at("Cannot read source", &source)?;
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
                match fs::symlink_metadata(&sidecar) {
                    Ok(_) => {
                        return Err(Error::new(
                            "SQLite snapshot has WAL/SHM sidecars; stop writers and export consistently",
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => {
                        return Err(Error::new(format!(
                            "Cannot inspect SQLite sidecar {}: {e}",
                            Path::new(&sidecar).display()
                        )));
                    }
                }
            }
        }
        if let Some(parent) = target.parent() {
            private_dir(parent)?;
        }
        let copied = if !shared && reuse_captured(config, relative_path, destination, &mut file)? {
            before.len()
        } else {
            file.seek(SeekFrom::Start(count as u64))?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .at("Cannot create export", &target)?;
            crate::state::private_file(&output, &target)?;
            output
                .write_all(&prefix[..count])
                .at("Cannot write export", &target)?;
            let copied = std::io::copy(&mut file, &mut output).at("Cannot copy source", &source)?
                + count as u64;
            preserve_metadata(&output, &before)?;
            copied
        };
        let after = file.metadata().at("Cannot inspect source", &source)?;
        if before.len() != copied || !unchanged(&before, &after)? {
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
    let mut file = File::open("/").at("Cannot open source root", Path::new("/"))?;
    // Resolve every component without following concurrent symlink replacements.
    while let Some(part) = parts.next() {
        let name = CString::new(part.as_bytes()).map_err(|_| Error::new("Invalid source path"))?;
        let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
        if parts.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(Error::new(format!(
                "Cannot open selected source without following symlinks: {}: {}",
                root.join(relative).display(),
                std::io::Error::last_os_error()
            )));
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
    for entry in fs::read_dir(root).at("Cannot read imported directory", root)? {
        let entry = entry.at("Cannot read imported entry", root)?;
        let kind = entry
            .file_type()
            .at("Cannot inspect imported entry", &entry.path())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn mandatory_exclusions_match_the_operations_document() {
        let document = include_str!("../OPS.md");
        for (heading, end, rules) in [
            ("- Exact names:", "- Prefixes:", EXCLUDED_NAMES),
            ("- Prefixes:", "- Suffixes:", EXCLUDED_PREFIXES),
            ("- Suffixes:", "- Also excluded:", EXCLUDED_SUFFIXES),
        ] {
            let section = document
                .split_once(heading)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0;
            let documented: BTreeSet<_> = section.split('`').skip(1).step_by(2).collect();
            let implemented: BTreeSet<_> = rules.iter().copied().collect();
            assert_eq!(documented, implemented, "{heading}");
            assert_eq!(implemented.len(), rules.len(), "duplicate {heading}");
        }
        let extra = GlobSetBuilder::new().build().unwrap();
        for path in [
            "notes/app.log.1",
            "models/pytorch_model-1.bin",
            "notes/.env",
        ] {
            assert!(excluded(Path::new(path), &extra));
        }
        assert!(!excluded(Path::new("notes/decision.md"), &extra));
    }
}
