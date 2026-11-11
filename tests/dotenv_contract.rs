#[allow(dead_code)]
mod common;

use common::*;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn without_passwords(fixture: &Fixture, bot: &Bot, args: &[&str]) -> Command {
    let mut command = fixture.cli_command(bot, args);
    for name in [
        "TEST_HUB_SHARED_PASSWORD",
        "TEST_HUB_ALPHA_PASSWORD",
        "TEST_HUB_BETA_PASSWORD",
    ] {
        command.env_remove(name);
    }
    command
}

fn passwords() -> String {
    format!("TEST_HUB_SHARED_PASSWORD={SHARED_PASSWORD}\nTEST_HUB_ALPHA_PASSWORD={BOT_PASSWORD}\n")
}

fn rejected_before_children(fixture: &Fixture, tool: &Path) {
    for operation in ["setup", "sync", "status", "validate"] {
        let output = fixture.cli(&fixture.alpha, &[operation]);
        failure(&output);
        assert!(
            !text(&output).contains("synthetic-dotenv-secret"),
            "dotenv content leaked"
        );
        assert!(
            fs::read(tool.join("calls")).unwrap().is_empty(),
            "{operation} started a child before rejecting .env"
        );
        for repository in ["shared", "bot"] {
            assert!(!Path::new(fixture.alpha.repository(repository)).exists());
        }
    }
}

#[test]
fn forwarded_child_names_are_rejected_as_repository_passwords_before_children() {
    let mut violations = Vec::new();
    for name in CONNECTION_ENV.iter().map(|(name, _)| *name).chain([
        "PATH",
        "SYSTEMROOT",
        "WINDIR",
        "RESTIC_CACERT",
        "RESTIC_TLS_CLIENT_CERT",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ]) {
        for provider in ["google-drive", "r2", "local"] {
            for repository in ["shared", "bot"] {
                let mut fixture = Fixture::new();
                let tool = Fixture::wrapper(&mut fixture.alpha);
                fs::write(tool.join("mode"), "offline").unwrap();
                fixture.alpha.config["storage"]["provider"] = provider.into();
                if provider == "r2" {
                    for repository in ["shared", "bot"] {
                        fixture.alpha.config["repositories"][repository]["url"] =
                            format!("s3:https://example.invalid/reserved/{repository}").into();
                    }
                }
                fixture.alpha.config["repositories"][repository]["password_env"] = name.into();
                fixture.alpha.save();
                for operation in ["setup", "sync", "status", "validate"] {
                    fs::write(tool.join("calls"), "").unwrap();
                    if tool.join("boundary").exists() {
                        fs::remove_file(tool.join("boundary")).unwrap();
                    }
                    let output = fixture.cli_env(
                        &fixture.alpha,
                        &[operation],
                        &[(name, "synthetic-reserved-password-secret")],
                    );
                    redacted(&output);
                    let output_text = text(&output);
                    for (valid, reason) in [
                        (!output.status.success(), "operation succeeded"),
                        (
                            output_text.contains("Invalid password environment variable name:")
                                && output_text.contains("reserved"),
                            "missing reserved-name rejection",
                        ),
                        (
                            !output_text.contains("synthetic-reserved-password-secret"),
                            "password leaked",
                        ),
                        (
                            fs::read(tool.join("calls")).unwrap().is_empty()
                                && !tool.join("boundary").exists(),
                            "started a child",
                        ),
                        (
                            ["shared", "bot"].into_iter().all(|repository| {
                                !Path::new(fixture.alpha.repository(repository)).exists()
                            }),
                            "created a repository",
                        ),
                    ] {
                        if !valid {
                            violations.push(format!(
                                "{provider}/{repository}/{name}/{operation}: {reason}"
                            ));
                        }
                    }
                }
            }
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn custom_repository_password_names_remain_allowed_and_isolated() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.alpha.config["repositories"]["shared"]["password_env"] =
        "CUSTOM_SHARED_PASSWORD".into();
    fixture.alpha.config["repositories"]["bot"]["password_env"] = "CUSTOM_BOT_PASSWORD".into();
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    let output = without_passwords(&fixture, &fixture.alpha, &["setup"])
        .env("CUSTOM_SHARED_PASSWORD", SHARED_PASSWORD)
        .env("CUSTOM_BOT_PASSWORD", BOT_PASSWORD)
        .output()
        .unwrap();
    success(&output);
    let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut delivered = Vec::new();
    for call in boundary.split("__CALL__\n").skip(1) {
        assert_eq!(child_env(call, "CUSTOM_SHARED_PASSWORD"), None);
        assert_eq!(child_env(call, "CUSTOM_BOT_PASSWORD"), None);
        if let Some(password) = child_env(call, "RESTIC_PASSWORD") {
            assert!([SHARED_PASSWORD, BOT_PASSWORD].contains(&password));
            delivered.push(password);
        }
    }
    assert!(delivered.contains(&SHARED_PASSWORD));
    assert!(delivered.contains(&BOT_PASSWORD));
}

#[test]
fn dotenv_passwords_support_real_setup_sync_and_byte_exact_restore() {
    let fixture = Fixture::new();
    let file = dotenv(
        &fixture.alpha,
        format!(
            "\n# Passwords delivered only by this file\nexport TEST_HUB_SHARED_PASSWORD=\"{SHARED_PASSWORD}\"\nTEST_HUB_ALPHA_PASSWORD=   {BOT_PASSWORD}   \n",
        ),
    );
    for operation in ["setup", "sync"] {
        success(
            &without_passwords(&fixture, &fixture.alpha, &[operation])
                .output()
                .unwrap(),
        );
    }
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(snapshots.len(), 1);
    let destination = fixture.temp.path().join("dotenv-recovery");
    success(
        &without_passwords(
            &fixture,
            &fixture.alpha,
            &[
                "restore",
                "--repository",
                "bot",
                "--snapshot",
                snapshots.first().unwrap(),
                "--destination",
                destination.to_str().unwrap(),
            ],
        )
        .output()
        .unwrap(),
    );
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
    assert_no_plaintext(&PathBuf::from(fixture.alpha.repository("shared")));
    assert_no_plaintext(&PathBuf::from(fixture.alpha.repository("bot")));
    assert_eq!(
        fs::metadata(file).unwrap().permissions().mode() & 0o7777,
        0o600
    );
}

#[test]
fn dotenv_values_are_literal_and_quoted_whitespace_is_preserved() {
    for (quote, prefix) in [("'", "'"), ("\"", "\""), ("", "synthetic")] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let marker = fixture.temp.path().join("must-not-execute");
        let shared = "  synthetic 'shared' \"password\" # value=日本語  ";
        let unquoted = "localhost,127.0.0.1 'single' \"double\" # literal=日本語 $EXPAND_ME \\n";
        let bot = format!(
            "{prefix} 'single' \"double\" $EXPAND_ME ${{EXPAND_ME}} $(touch {}) `touch {}` \\n # value=日本語",
            marker.display(),
            marker.display()
        );
        dotenv(
            &fixture.alpha,
            format!(
                "  # full-line comment\r\nexport TEST_HUB_SHARED_PASSWORD='{shared}'\r\nTEST_HUB_ALPHA_PASSWORD={quote}{bot}{quote}\r\nNO_PROXY=   {unquoted}   \r\n",
            ),
        );
        let output = without_passwords(&fixture, &fixture.alpha, &["setup"])
            .env("EXPAND_ME", "wrong-expanded-value")
            .output()
            .unwrap();
        success(&output);
        let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
        let delivered: Vec<_> = boundary
            .split("__CALL__\n")
            .skip(1)
            .filter_map(|call| child_env(call, "RESTIC_PASSWORD"))
            .collect();
        assert!(delivered.contains(&shared));
        assert!(delivered.contains(&bot.as_str()));
        for call in boundary.split("__CALL__\n").skip(1) {
            assert_eq!(child_env(call, "NO_PROXY"), Some(unquoted));
        }
        assert!(!text(&output).contains(shared));
        assert!(!text(&output).contains(&bot));
        assert!(!marker.exists());
    }
}

#[test]
fn dotenv_process_values_override_file_values_including_empty_connections() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    dotenv(
        &fixture.alpha,
        format!(
            "TEST_HUB_SHARED_PASSWORD={SHARED_PASSWORD}\nTEST_HUB_ALPHA_PASSWORD=synthetic-wrong-file-password\nHTTP_PROXY=http://file-proxy.example.invalid:8080\nHTTPS_PROXY=http://file-proxy.example.invalid:8081\n",
        ),
    );
    let output = without_passwords(&fixture, &fixture.alpha, &["setup"])
        .env("TEST_HUB_ALPHA_PASSWORD", BOT_PASSWORD)
        .env("HTTP_PROXY", "http://process-proxy.example.invalid:8082")
        .env("HTTPS_PROXY", "")
        .output()
        .unwrap();
    success(&output);
    let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut saw_bot = false;
    for call in boundary.split("__CALL__\n").skip(1) {
        assert_eq!(
            child_env(call, "HTTP_PROXY"),
            Some("http://process-proxy.example.invalid:8082")
        );
        assert_eq!(child_env(call, "HTTPS_PROXY"), Some(""));
        if let Some(password) = child_env(call, "RESTIC_PASSWORD") {
            assert!([SHARED_PASSWORD, BOT_PASSWORD].contains(&password));
            saw_bot |= password == BOT_PASSWORD;
        }
    }
    assert!(saw_bot);
}

#[test]
fn dotenv_proxy_case_pairs_use_original_process_presence_including_empty_values() {
    let pairs = [
        ("HTTP_PROXY", "http_proxy"),
        ("HTTPS_PROXY", "https_proxy"),
        ("ALL_PROXY", "all_proxy"),
        ("NO_PROXY", "no_proxy"),
    ];
    let mut mismatches = BTreeSet::new();
    for (upper, lower) in [
        (None, None),
        (Some(""), None),
        (Some("http://process-upper.example.invalid:8080"), None),
        (None, Some("")),
        (None, Some("http://process-lower.example.invalid:8081")),
        (Some(""), Some("")),
        (Some(""), Some("http://process-lower.example.invalid:8081")),
        (Some("http://process-upper.example.invalid:8080"), Some("")),
        (
            Some("http://process-upper.example.invalid:8080"),
            Some("http://process-lower.example.invalid:8081"),
        ),
    ] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let mut document = passwords();
        for (name, value) in CONNECTION_ENV {
            document.push_str(&format!("{name}={value}\n"));
        }
        dotenv(&fixture.alpha, document);
        let mut command = without_passwords(&fixture, &fixture.alpha, &["setup"]);
        command.env("test_hub_shared_password", "synthetic-other-case-password");
        for name in ["ssl_cert_file", "ssl_cert_dir", "curl_ca_bundle"] {
            command.env(name, "/synthetic/process-other-case-ca");
        }
        for (upper_name, lower_name) in pairs {
            for (name, value) in [(upper_name, upper), (lower_name, lower)] {
                if let Some(value) = value {
                    command.env(name, value);
                }
            }
        }
        success(&command.output().unwrap());
        let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
        assert!(boundary.contains("__CALL__\n"));
        for call in boundary.split("__CALL__\n").skip(1) {
            for (name, file_value) in CONNECTION_ENV {
                let process_present = upper.is_some() || lower.is_some();
                let expected = match name {
                    "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY" if process_present => {
                        upper
                    }
                    "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy" if process_present => {
                        lower
                    }
                    _ => Some(file_value),
                };
                let actual = child_env(call, name);
                if actual != expected {
                    mismatches.insert(format!(
                        "{upper:?}/{lower:?}/{name}: expected {expected:?}, got {actual:?}"
                    ));
                }
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

#[test]
fn dotenv_does_not_replace_an_empty_process_password() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    dotenv(&fixture.alpha, passwords());
    let output = fixture.cli_env(
        &fixture.alpha,
        &["setup"],
        &[("TEST_HUB_ALPHA_PASSWORD", "")],
    );
    failure(&output);
    assert_no_writes(&tool);
    for repository in ["shared", "bot"] {
        assert!(!Path::new(fixture.alpha.repository(repository)).exists());
    }
}

#[test]
fn dotenv_resolves_beside_canonical_config_not_cwd_or_parent() {
    let fixture = Fixture::new();
    dotenv(&fixture.alpha, passwords());
    let alias_directory = fixture.temp.path().join("config-alias");
    let cwd = fixture.temp.path().join("other-cwd");
    fs::create_dir(&alias_directory).unwrap();
    fs::create_dir(&cwd).unwrap();
    for directory in [
        fixture.temp.path(),
        alias_directory.as_path(),
        cwd.as_path(),
    ] {
        fs::write(directory.join(".env"), b"invalid synthetic-dotenv-secret").unwrap();
    }
    let selected = Bot {
        file: alias_directory.join("config.yaml"),
        config: fixture.alpha.config.clone(),
    };
    symlink(&fixture.alpha.file, &selected.file).unwrap();
    success(
        &without_passwords(&fixture, &selected, &["setup"])
            .current_dir(&cwd)
            .output()
            .unwrap(),
    );
    assert!(!fixture.snapshots(&fixture.alpha, "shared").is_empty());
}

#[test]
fn dotenv_does_not_search_cwd_or_parent_when_adjacent_file_is_missing() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let parent = Bot {
        file: fixture.temp.path().join("unused-config.yaml"),
        config: fixture.alpha.config.clone(),
    };
    dotenv(&parent, passwords());
    failure(
        &without_passwords(&fixture, &fixture.alpha, &["setup"])
            .output()
            .unwrap(),
    );
    assert_no_writes(&tool);
    for repository in ["shared", "bot"] {
        assert!(!Path::new(fixture.alpha.repository(repository)).exists());
    }
}

#[test]
fn dotenv_rejects_permissions_other_than_exactly_0600_before_children() {
    for mode in [
        0o000, 0o400, 0o640, 0o644, 0o660, 0o700, 0o1600, 0o2600, 0o4600,
    ] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let file = dotenv(&fixture.alpha, "TEST_HUB_OPAQUE=synthetic-dotenv-secret\n");
        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
        rejected_before_children(&fixture, &tool);
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
}

#[test]
fn dotenv_rejects_existing_and_dangling_symlinks_before_children() {
    for dangling in [false, true] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let target = fixture.temp.path().join("private-secrets");
        if !dangling {
            fs::write(&target, "TEST_HUB_OPAQUE=synthetic-dotenv-secret\n").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
        symlink(&target, fixture.alpha.file.parent().unwrap().join(".env")).unwrap();
        rejected_before_children(&fixture, &tool);
        if !dangling {
            assert_eq!(
                fs::read_to_string(target).unwrap(),
                "TEST_HUB_OPAQUE=synthetic-dotenv-secret\n"
            );
        }
    }
}

#[test]
fn dotenv_rejects_nonregular_files_before_children() {
    for socket in [false, true] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let file = fixture.alpha.file.parent().unwrap().join(".env");
        let _listener = if socket {
            Some(UnixListener::bind(&file).unwrap())
        } else {
            fs::create_dir(&file).unwrap();
            None
        };
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        rejected_before_children(&fixture, &tool);
        if !socket {
            fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}

#[test]
fn dotenv_rejects_invalid_syntax_without_leaking_or_starting_children() {
    for invalid in [
        "missing-equals synthetic-dotenv-secret\n".as_bytes(),
        b"9INVALID=synthetic-dotenv-secret\n",
        b"INVALID-NAME=synthetic-dotenv-secret\n",
        "日本語=synthetic-dotenv-secret\n".as_bytes(),
        b"TEST_HUB_OPAQUE='synthetic-dotenv-secret\n",
        b"TEST_HUB_OPAQUE=\"synthetic-dotenv-secret\n",
        b"TEST_HUB_OPAQUE='synthetic-dotenv-secret' trailing\n",
        b"TEST_HUB_OPAQUE='synthetic-dotenv-secret\nsecond line'\n",
        b"TEST_HUB_OPAQUE=synthetic-dotenv-secret\nexport TEST_HUB_OPAQUE=duplicate\n",
        b"TEST_HUB_OPAQUE=synthetic-dotenv-secret\0suffix\n",
        b"BAD\0NAME=synthetic-dotenv-secret\n",
        b"TEST_HUB_OPAQUE=synthetic-dotenv-secret\xff\n",
    ] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let mut bytes = passwords().into_bytes();
        bytes.extend_from_slice(invalid);
        dotenv(&fixture.alpha, bytes);
        rejected_before_children(&fixture, &tool);
    }
}
