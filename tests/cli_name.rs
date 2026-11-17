#[path = "common/temp.rs"]
mod temp;

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use temp::TempDir;

const UTSURIGI: &str = env!("CARGO_BIN_EXE_utsurigi");
// The compatibility command that existing scheduled jobs and Bot instructions still call.
const UTSURIGI_AGENT_HUB: &str = env!("CARGO_BIN_EXE_utsurigi-agent-hub");

fn assert_version(command: &mut Command, name: &str) {
    let output = command.arg("--version").output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}: {stderr}", output.status);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{name} {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn utsurigi_reports_its_name_and_version() {
    assert_version(Command::new(UTSURIGI).env_clear(), "utsurigi");
}

#[test]
fn utsurigi_agent_hub_still_reports_its_name_and_version() {
    assert_version(
        Command::new(UTSURIGI_AGENT_HUB).env_clear(),
        "utsurigi-agent-hub",
    );
}

#[test]
fn utsurigi_agent_hub_gives_the_same_stderr_and_exit_code() {
    let temp = TempDir::new();
    // Not UTF-8: both names must accept it.
    let missing = temp.path().join(OsStr::from_bytes(b"missing-\xff.yaml"));
    let run = |program: &str| {
        Command::new(program)
            .env_clear()
            .arg("--config")
            .arg(&missing)
            .arg("status")
            .output()
            .unwrap()
    };
    let expected = run(UTSURIGI);
    let actual = run(UTSURIGI_AGENT_HUB);
    assert_ne!(expected.status.code(), Some(0));
    assert_eq!(actual.status.code(), expected.status.code());
    assert_eq!(
        String::from_utf8_lossy(&actual.stderr),
        String::from_utf8_lossy(&expected.stderr)
    );
}

#[test]
fn a_lone_utsurigi_agent_hub_copy_runs_standalone() {
    let temp = TempDir::new();
    let alone = temp.path().join("utsurigi-agent-hub");
    // cp, not fs::copy: a write handle open here could leak into a concurrent test's
    // fork and make the exec below fail with ETXTBSY.
    let copied = Command::new("cp")
        .arg(UTSURIGI_AGENT_HUB)
        .arg(&alone)
        .status()
        .unwrap();
    assert!(copied.success());
    // No utsurigi beside it and no PATH: copying the old command alone must keep working.
    assert_version(Command::new(alone).env_clear(), "utsurigi-agent-hub");
}
