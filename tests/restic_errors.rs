#[allow(dead_code)]
mod common;

use common::*;
use std::fs;
use std::path::Path;

fn failing_restic(fixture: &mut Fixture, code: i32, stderr: &[u8]) {
    let directory = Fixture::wrapper(&mut fixture.alpha);
    fs::write(directory.join("stderr"), stderr).unwrap();
    fs::write(directory.join("exit-code"), code.to_string()).unwrap();
    fs::write(
        directory.join("restic"),
        r#"#!/bin/sh
set -eu
directory=${0%/*}
if [ "$1" = version ]; then exec "$directory/real-restic" "$@"; fi
cat "$directory/stderr" >&2
exit "$(cat "$directory/exit-code")"
"#,
    )
    .unwrap();
}

fn restore(fixture: &Fixture, repository: &str, env: &[(&str, &str)]) -> std::process::Output {
    fixture.cli_env(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            repository,
            "--snapshot",
            &"a".repeat(64),
            "--destination",
            fixture.temp.path().join("restore").to_str().unwrap(),
        ],
        env,
    )
}

fn assert_exit_meaning(code: i32, meaning: &str) {
    let mut fixture = Fixture::new();
    failing_restic(
        &mut fixture,
        code,
        b"Fatal: synthetic failure\nignored second line\n",
    );
    for repository in ["shared", "bot"] {
        let output = restore(&fixture, repository, &[]);
        failure(&output);
        assert_eq!(output.status.code(), Some(code));
        let message = text(&output);
        assert!(
            message.contains(&format!("{repository} repository")),
            "{message}"
        );
        assert!(message.contains(&format!("exit {code}")), "{message}");
        assert!(message.contains(meaning), "{message}");
        assert!(message.contains("Fatal: synthetic failure"), "{message}");
        assert!(!message.contains("ignored second line"), "{message}");
        assert!(!Path::new(fixture.alpha.repository(repository)).exists());
    }
}

#[test]
fn exit_3_explains_unreadable_sources_in_each_repository() {
    assert_exit_meaning(3, "source files could not be read");
}

#[test]
fn exit_10_explains_missing_repository_in_each_repository() {
    assert_exit_meaning(10, "repository does not exist");
}

#[test]
fn exit_11_explains_repository_lock_in_each_repository() {
    assert_exit_meaning(11, "repository is locked");
}

#[test]
fn exit_12_explains_incorrect_password_in_each_repository() {
    assert_exit_meaning(12, "incorrect password");
}

#[test]
fn stderr_redacts_known_secrets_urls_and_unlisted_credentials() {
    let mut fixture = Fixture::new();
    let access = "dummy-access-key-27";
    let secret = "dummy-secret-key-28";
    let token = "dummy-session-token-29";
    let diagnostic = format!(
        "Fatal: synthetic failure {SHARED_PASSWORD} {BOT_PASSWORD} {PEER_PASSWORD} {access} {secret} {token} {} {} op://synthetic/shared/password https://alice:unlisted-password@trace.example.invalid/private?token=unlisted-token password = unlisted-password\nsecond-line-private\n",
        fixture.alpha.repository("shared"),
        fixture.alpha.repository("bot"),
    );
    failing_restic(&mut fixture, 12, diagnostic.as_bytes());
    let output = restore(
        &fixture,
        "bot",
        &[
            ("AWS_ACCESS_KEY_ID", access),
            ("AWS_SECRET_ACCESS_KEY", secret),
            ("AWS_SESSION_TOKEN", token),
        ],
    );
    failure(&output);
    let message = text(&output);
    assert!(message.contains("Fatal: synthetic failure"), "{message}");
    for hidden in [
        access,
        secret,
        token,
        fixture.alpha.repository("shared"),
        fixture.alpha.repository("bot"),
        "op://synthetic/shared/password",
        "alice",
        "unlisted-password",
        "unlisted-token",
        "trace.example.invalid",
        "second-line-private",
    ] {
        assert!(
            !message.contains(hidden),
            "diagnostic exposed dummy private data"
        );
    }
}

#[test]
fn stderr_is_bounded_and_contains_no_terminal_controls() {
    let mut fixture = Fixture::new();
    let diagnostic = format!(
        "Fatal: synthetic failure\u{1b}[31m\t{}\nprivate second line",
        "界".repeat(2000)
    );
    failing_restic(&mut fixture, 42, diagnostic.as_bytes());
    let output = restore(&fixture, "shared", &[]);
    failure(&output);
    assert_eq!(output.status.code(), Some(42));
    let message = text(&output);
    assert!(message.contains("Fatal: synthetic failure"), "{message}");
    assert!(message.len() <= 1024, "diagnostic must be bounded in bytes");
    assert!(
        message
            .trim_end_matches('\n')
            .chars()
            .all(|c| !c.is_control())
    );
    assert!(!message.contains("private second line"));
}
