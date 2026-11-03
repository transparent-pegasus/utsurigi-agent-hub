#[allow(dead_code)]
mod common;

use common::*;
use std::fs;
use std::path::{Path, PathBuf};

fn failing_restic(fixture: &mut Fixture, code: i32, stderr: &[u8]) -> PathBuf {
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
    directory
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
    let proxy_user = "synthetic-proxy-user-30";
    let proxy_password = "synthetic-proxy-password-31";
    let proxy = format!("http://{proxy_user}:{proxy_password}@proxy.example.invalid:8080");
    let diagnostic = format!(
        "Fatal: synthetic failure {SHARED_PASSWORD} {BOT_PASSWORD} {PEER_PASSWORD} {access} {secret} {token} {proxy} {} {} op://synthetic/shared/password https://alice:unlisted-password@trace.example.invalid/private?token=unlisted-token password = unlisted-password\nsecond-line-private\n",
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
            ("HTTPS_PROXY", &proxy),
        ],
    );
    failure(&output);
    let message = text(&output);
    assert!(message.contains("Fatal: synthetic failure"), "{message}");
    for hidden in [
        access,
        secret,
        token,
        proxy_user,
        proxy_password,
        "proxy.example.invalid",
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
fn stderr_redacts_dotenv_custom_password_aws_secret_and_proxy_credentials() {
    let mut fixture = Fixture::new();
    let password = "synthetic-dotenv-custom-password-41";
    let aws_secret = "synthetic-dotenv-aws-secret-42";
    let proxy_user = "synthetic-dotenv-proxy-user-43";
    let proxy_password = "synthetic-dotenv-proxy-password-44";
    let proxy = format!("http://{proxy_user}:{proxy_password}@proxy.example.invalid:8080");
    fixture.alpha.config["repositories"]["bot"]["password_env"] = "DOTENV_REPOSITORY_KEY".into();
    fixture.alpha.save();
    dotenv(
        &fixture.alpha,
        format!(
            "TEST_HUB_SHARED_PASSWORD={SHARED_PASSWORD}\nDOTENV_REPOSITORY_KEY={password}\nAWS_SECRET_ACCESS_KEY={aws_secret}\nHTTPS_PROXY={proxy}\n",
        ),
    );
    failing_restic(
        &mut fixture,
        42,
        format!("Fatal: dotenv failure {password} {aws_secret} {proxy}\n").as_bytes(),
    );
    let mut command = fixture.cli_command(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            &"a".repeat(64),
            "--destination",
            fixture.temp.path().join("dotenv-restore").to_str().unwrap(),
        ],
    );
    for name in [
        "TEST_HUB_SHARED_PASSWORD",
        "TEST_HUB_ALPHA_PASSWORD",
        "TEST_HUB_BETA_PASSWORD",
    ] {
        command.env_remove(name);
    }
    let output = command.output().unwrap();
    assert_eq!(output.status.code(), Some(42));
    let message = text(&output);
    assert!(
        message.contains("Fatal: dotenv failure"),
        "restic diagnostic was not emitted"
    );
    for (kind, hidden) in [
        ("repository password", password),
        ("AWS secret", aws_secret),
        ("proxy user", proxy_user),
        ("proxy password", proxy_password),
    ] {
        assert!(
            !message.contains(hidden),
            "diagnostic exposed a dotenv {kind}"
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
        stderr_diagnostic(&output).len() <= 512,
        "stderr diagnostic must fit within 512 bytes including ellipsis"
    );
    assert!(
        message
            .trim_end_matches('\n')
            .chars()
            .all(|c| !c.is_control())
    );
    assert!(!message.contains("private second line"));
}

fn stderr_diagnostic(output: &std::process::Output) -> String {
    text(output)
        .split_once("stderr: ")
        .expect("restic failure must include stderr")
        .1
        .trim_end_matches('\n')
        .to_owned()
}

#[test]
fn short_password_preserves_exit_error_but_redacts_standalone_values() {
    let mut fixture = Fixture::new();
    failing_restic(
        &mut fixture,
        42,
        b"Fatal: exit_error repository index unavailable; x (x) 'x' xray next\n",
    );
    let output = restore(&fixture, "bot", &[("TEST_HUB_ALPHA_PASSWORD", "x")]);
    failure(&output);
    assert_eq!(output.status.code(), Some(42));
    let diagnostic = stderr_diagnostic(&output);
    assert!(
        diagnostic.starts_with("Fatal: exit_error repository index unavailable;"),
        "{diagnostic}"
    );
    assert!(diagnostic.ends_with("xray next"), "{diagnostic}");
    for token in [" x ", "(x)", "'x'"] {
        assert!(!diagnostic.contains(token), "short credential exposed");
    }
    assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
}

#[test]
fn wrong_password_explanation_and_password_prose_remain_readable() {
    let mut fixture = Fixture::new();
    let directory = failing_restic(&mut fixture, 12, b"");
    for diagnostic in [
        "Fatal: wrong password or no key found",
        "Fatal: password is incorrect; check repository credentials",
        "Fatal: passwordless repository cannot be opened",
        "Fatal: cannot load password file: permission denied",
    ] {
        fs::write(directory.join("stderr"), diagnostic).unwrap();
        let output = restore(&fixture, "shared", &[]);
        failure(&output);
        assert_eq!(stderr_diagnostic(&output), diagnostic);
    }
}

#[test]
fn credential_fields_redact_values_and_preserve_explicit_boundaries() {
    let mut fixture = Fixture::new();
    let directory = failing_restic(&mut fixture, 42, b"");
    for (field, preserve_suffix) in [
        ("password=unlisted-value", false),
        ("PASSWORD = unlisted-value", false),
        ("password : unlisted-value", false),
        ("password = 'unlisted value with spaces'", true),
        (r#"password = "unlisted value with spaces""#, true),
        (r#"password = "unlisted \" quoted value""#, true),
        (r#""api_token": "unlisted value with spaces""#, true),
        ("'client_secret' : 'unlisted value with spaces'", true),
        ("password = 'unlisted,value;with spaces'", true),
        (r#""api_token": "unlisted,value;with spaces""#, true),
        ("custom_credential = unlisted value with spaces", false),
        ("password 'unlisted value with spaces'", true),
        ("password unlisted value with spaces", false),
        ("CUSTOM_API_TOKEN unlisted-value", false),
        ("AWS_ACCESS_KEY_ID unlisted-value", false),
        ("api-key unlisted-value", false),
        ("Authorization unlisted-value", false),
        ("--password unlisted-value", false),
        ("--access-key 'unlisted value with spaces'", true),
        ("Authorization: Bearer unlisted-value", false),
        ("Bearer unlisted-value", false),
        ("bearer 'unlisted value with spaces'", true),
    ] {
        let diagnostic = format!("Fatal: {field}; repository is locked");
        fs::write(directory.join("stderr"), diagnostic).unwrap();
        let output = restore(&fixture, "shared", &[]);
        failure(&output);
        let diagnostic = stderr_diagnostic(&output);
        for hidden in ["unlisted", "value", "spaces", "quoted"] {
            assert!(!diagnostic.contains(hidden), "credential exposed: {field}");
        }
        assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
        assert!(diagnostic.starts_with("Fatal: "), "{diagnostic}");
        if preserve_suffix {
            assert!(
                diagnostic.ends_with("; repository is locked"),
                "{diagnostic}"
            );
        }
    }
}

fn assert_unquoted_credential_is_hidden(value: &str, trailing_fragment: &str) {
    let mut fixture = Fixture::new();
    let directory = failing_restic(&mut fixture, 42, b"");
    for field in [
        format!("password={value}"),
        format!("custom_credential: {value}"),
        format!("password {value}"),
        format!("--password {value}"),
        format!("Authorization: Bearer {value}"),
        format!("Bearer {value}"),
    ] {
        fs::write(
            directory.join("stderr"),
            format!("Fatal: exit_error repository index unavailable; {field}; retry later"),
        )
        .unwrap();
        let output = restore(&fixture, "shared", &[]);
        failure(&output);
        assert_eq!(output.status.code(), Some(42));
        let message = text(&output);
        assert!(message.contains("shared repository"), "{message}");
        let diagnostic = stderr_diagnostic(&output);
        assert!(
            diagnostic.starts_with("Fatal: exit_error repository index unavailable;"),
            "{diagnostic}"
        );
        for hidden in ["unlisted", trailing_fragment] {
            assert!(
                !diagnostic.contains(hidden),
                "credential fragment exposed: {field}: {diagnostic}"
            );
        }
        assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
    }
}

#[test]
fn unquoted_credentials_containing_commas_are_hidden() {
    assert_unquoted_credential_is_hidden("unlisted,comma-tail", "comma-tail");
}

#[test]
fn unquoted_credentials_containing_semicolons_are_hidden() {
    assert_unquoted_credential_is_hidden("unlisted;semicolon-tail", "semicolon-tail");
}

#[test]
fn known_delimited_credentials_are_hidden_in_unquoted_fields() {
    let mut fixture = Fixture::new();
    let directory = failing_restic(&mut fixture, 42, b"");
    for value in ["known-head,known-tail", "known-head;known-tail"] {
        for field in [
            format!("password={value}"),
            format!("password={value}-suffix"),
            format!("password {value}-suffix"),
            format!("Bearer {value}-suffix"),
        ] {
            fs::write(
                directory.join("stderr"),
                format!("Fatal: exit_error repository index unavailable; {field}; retry later"),
            )
            .unwrap();
            let output = restore(&fixture, "bot", &[("TEST_HUB_ALPHA_PASSWORD", value)]);
            failure(&output);
            assert_eq!(output.status.code(), Some(42));
            let message = text(&output);
            assert!(message.contains("bot repository"), "{message}");
            let diagnostic = stderr_diagnostic(&output);
            assert!(
                diagnostic.starts_with("Fatal: exit_error repository index unavailable;"),
                "{diagnostic}"
            );
            for hidden in ["known-head", "known-tail", "suffix"] {
                assert!(
                    !diagnostic.contains(hidden),
                    "known credential fragment exposed: {field}: {diagnostic}"
                );
            }
            assert!(diagnostic.contains("[redacted]"), "{diagnostic}");
        }
    }
}

#[test]
fn short_credentials_are_hidden_in_assignments_bearer_and_urls() {
    let mut fixture = Fixture::new();
    let directory = failing_restic(&mut fixture, 42, b"");
    for (field, preserve_suffix) in [
        ("password=x", false),
        ("password = 'x'", true),
        ("password x", false),
        ("Authorization: Bearer x", false),
        ("Bearer x", false),
        ("password=x,unlisted", false),
        ("password=x;unlisted", false),
        ("password x,unlisted", false),
        ("Bearer x;unlisted", false),
        ("password = 'x,unlisted;value'", true),
        (
            "https://alice:x@trace.example.invalid/private?token=x",
            false,
        ),
        ("alice:x@trace.example.invalid", true),
        ("s3:trace.example.invalid/private", true),
    ] {
        fs::write(
            directory.join("stderr"),
            format!("Fatal: {field}; retry later"),
        )
        .unwrap();
        let output = restore(&fixture, "bot", &[("TEST_HUB_ALPHA_PASSWORD", "x")]);
        failure(&output);
        let diagnostic = stderr_diagnostic(&output);
        for hidden in [
            "=x",
            "'x'",
            " x",
            "alice",
            "trace.example.invalid",
            "private",
            "unlisted",
            "value",
        ] {
            assert!(!diagnostic.contains(hidden), "credential exposed: {field}");
        }
        if preserve_suffix {
            assert!(
                diagnostic.ends_with("; retry later"),
                "{field}: {diagnostic}"
            );
        }
    }
}

#[test]
fn unicode_known_values_use_boundaries_and_configured_password_environment() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["repositories"]["bot"]["password_env"] = "TEST_HUB_OPAQUE".into();
    fixture.alpha.save();
    failing_restic(
        &mut fixture,
        42,
        "Fatal: 世界 index failed; 界 'two words'; retry later".as_bytes(),
    );
    let output = restore(
        &fixture,
        "bot",
        &[("TEST_HUB_OPAQUE", "界"), ("TEST_HUB_TOKEN", "two words")],
    );
    failure(&output);
    let diagnostic = stderr_diagnostic(&output);
    assert!(
        diagnostic.starts_with("Fatal: 世界 index failed;"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("; 界"),
        "short Unicode credential exposed"
    );
    assert!(
        !diagnostic.contains("two words"),
        "environment credential exposed"
    );
    assert!(diagnostic.ends_with("; retry later"), "{diagnostic}");
}
