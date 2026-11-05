use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub struct TempDir(tempfile::TempDir);

impl TempDir {
    pub fn new() -> Self {
        Self(
            tempfile::Builder::new()
                .prefix("utsurigi-agent-hub-test-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap(),
        )
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}
