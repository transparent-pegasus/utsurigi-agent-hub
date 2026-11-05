use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn write_pem(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, include_bytes!("../fixtures/restic-tls.pem")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}
