#[path = "common/restic.rs"]
mod restic;
#[path = "common/temp.rs"]
mod temp;

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};
use temp::TempDir;

fn command(executable: &Path, directory: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("RESTIC_PASSWORD", "synthetic-fixture-check-password")
        .env("RESTIC_CACHE_DIR", directory.join("cache"))
        .current_dir(directory.join("source"));
    command
}

fn ok(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn real_restic_fixture_preserves_history_and_injects_failures_after_real_writes() {
    let temp = TempDir::new();
    let root = temp.path();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    let file = source.join("日本語.md");
    fs::write(&file, b"version one").unwrap();
    let executable = restic::executable();
    assert!(executable.is_absolute());
    assert_eq!(executable.canonicalize().unwrap(), executable);
    symlink(executable, root.join("real-restic")).unwrap();
    let wrapper = root.join("restic");
    fs::write(&wrapper, include_str!("fixtures/restic-wrapper.sh")).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("mode"), "pass").unwrap();
    let repository = root.join("repository");
    let invoke = |args: &[&str]| {
        command(&wrapper, root)
            .arg("--repo")
            .arg(&repository)
            .args(args)
            .output()
            .unwrap()
    };
    let version = ok(invoke(&["version"]));
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("restic 0.19.1 "));
    ok(invoke(&["init"]));
    ok(invoke(&["backup", "."]));
    let first = ok(invoke(&["snapshots", "--json"]));
    assert_eq!(
        String::from_utf8_lossy(&first.stdout)
            .matches("\"id\":")
            .count(),
        1
    );

    fs::write(&file, b"version two").unwrap();
    fs::write(root.join("mode"), "partial").unwrap();
    assert_eq!(invoke(&["backup", "."]).status.code(), Some(3));
    let second = ok(invoke(&["snapshots", "--json"]));
    assert_eq!(
        String::from_utf8_lossy(&second.stdout)
            .matches("\"id\":")
            .count(),
        2
    );

    fs::write(root.join("mode"), "restore-failure").unwrap();
    let recovery = root.join("recovery");
    assert!(
        !invoke(&["restore", "latest", "--target", recovery.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(
        fs::read(recovery.join("日本語.md")).unwrap(),
        b"version two"
    );

    fs::write(&file, b"version three").unwrap();
    fs::write(root.join("mode"), "ambiguous").unwrap();
    assert_eq!(invoke(&["backup", "."]).status.code(), Some(1));
    let third = ok(invoke(&["snapshots", "--json"]));
    assert_eq!(
        String::from_utf8_lossy(&third.stdout)
            .matches("\"id\":")
            .count(),
        3
    );

    fs::write(root.join("mode"), "network").unwrap();
    assert_eq!(invoke(&["backup", "."]).status.code(), Some(1));
    assert_eq!(ok(invoke(&["snapshots", "--json"])).stdout, third.stdout);

    fs::write(root.join("mode"), "pass").unwrap();
    fs::write(root.join("repo-shared"), repository.to_str().unwrap()).unwrap();
    fs::write(root.join("repo-bot"), repository.to_str().unwrap()).unwrap();
    let mapped_env = ok(command(&wrapper, root)
        .env(
            "RESTIC_REPOSITORY",
            "s3:https://example.invalid/fake/shared",
        )
        .args(["snapshots", "--json"])
        .output()
        .unwrap());
    assert_eq!(mapped_env.stdout, third.stdout);
    let mapped_arg = ok(command(&wrapper, root)
        .args([
            "--repo=s3:https://example.invalid/fake/alpha",
            "snapshots",
            "--json",
        ])
        .output()
        .unwrap());
    assert_eq!(mapped_arg.stdout, third.stdout);
    let calls = fs::read_to_string(root.join("calls")).unwrap();
    assert!(!calls.lines().any(|call| call == "unlock"));
}
