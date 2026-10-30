use std::env;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub fn executable() -> PathBuf {
    let path = env::var_os("UTSURIGI_AGENT_HUB_TEST_RESTIC")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::split_paths(&env::var_os("PATH").unwrap_or_default())
                .map(|directory| directory.join("restic"))
                .find(|path| {
                    path.metadata().is_ok_and(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                })
                .expect("restic must be on PATH or set with UTSURIGI_AGENT_HUB_TEST_RESTIC; tests cannot skip it")
        });
    let path = path
        .canonicalize()
        .expect("UTSURIGI_AGENT_HUB_TEST_RESTIC or PATH must resolve to a real restic executable");
    assert!(
        path.metadata().is_ok_and(|metadata| {
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }),
        "restic must be an executable file; tests cannot skip it"
    );
    path
}
