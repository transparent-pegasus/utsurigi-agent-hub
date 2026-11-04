mod common;

use chrono::{DateTime, Timelike};
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

fn restore(
    fixture: &Fixture,
    repository: &str,
    snapshot: &str,
    destination: &Path,
) -> std::process::Output {
    fixture.cli(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            repository,
            "--snapshot",
            snapshot,
            "--destination",
            destination.to_str().unwrap(),
        ],
    )
}

fn payload_at(destination: &Path) {
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
}

fn status_timestamp(value: &Value) -> i64 {
    DateTime::parse_from_rfc3339(value.as_str().expect("status time must be a string"))
        .expect("status time must be RFC3339")
        .timestamp()
}

fn schedule_fields(status: &Value) -> (i64, i64) {
    (
        status_timestamp(&status["anchor"]),
        status_timestamp(&status["next_due"]),
    )
}

fn progress(status: &Value) -> Value {
    json!({
        "hub": status["hub"],
        "ready": status["ready"], "last_success": status["last_success"],
        "anchor": status["anchor"], "next_due": status["next_due"]
    })
}

// Failed operations may record the new structured failure, while every other
// persisted field must retain its value. Status-only and malformed-state tests
// continue to require byte-for-byte preservation.
fn assert_state_data_unchanged(bot: &Bot, previous: &[u8], operation: &str) {
    let bytes = fs::read(bot.path("state").join("status.json")).unwrap();
    let rendered = String::from_utf8(bytes.clone()).unwrap();
    for secret in [SHARED_PASSWORD, BOT_PASSWORD, PEER_PASSWORD] {
        assert!(!rendered.contains(secret), "failure state exposed a secret");
    }
    assert!(!rendered.contains("ownerless-raw-stderr-sentinel"));
    let mut before: Value = serde_json::from_slice(previous).unwrap();
    let mut after: Value = serde_json::from_slice(&bytes).unwrap();
    let mut previous_failures = before
        .as_object_mut()
        .unwrap()
        .remove("failures")
        .unwrap_or(json!({}));
    let mut current_failures = after
        .as_object_mut()
        .unwrap()
        .remove("failures")
        .expect("failed operation must persist its failure map");
    for failures in [&previous_failures, &current_failures] {
        for (name, record) in failures.as_object().expect("failures must be a map") {
            assert!(
                ["setup", "sync", "publish", "pull", "restore", "prune"].contains(&name.as_str())
            );
            assert_eq!(
                record.as_object().unwrap().len(),
                5,
                "failure schema is strict"
            );
            assert_eq!(record["operation"], *name);
            for field in ["operation", "category", "reason", "next_action"] {
                assert!(
                    record[field]
                        .as_str()
                        .is_some_and(|value| !value.trim().is_empty()),
                    "failure field {field} must be a nonempty string"
                );
            }
            assert!(record["recorded_at"].as_i64().is_some());
        }
    }
    previous_failures.as_object_mut().unwrap().remove(operation);
    assert!(
        current_failures
            .as_object_mut()
            .unwrap()
            .remove(operation)
            .is_some(),
        "failed operation must record its own entry"
    );
    assert_eq!(
        current_failures, previous_failures,
        "unrelated failures must remain intact"
    );
    assert_eq!(
        after, before,
        "failure recording must preserve every successful state field, joined and schedule"
    );
}

fn option<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then_some(pair[1]))
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix(&format!("{name}=")))
        })
}

#[test]
fn encrypted_round_trip_preserves_unicode_and_setup_is_repeatable() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["validate"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(snapshots.len(), 1);
    for repository in ["bot", "shared"] {
        success(&fixture.restic(&fixture.alpha, repository, &["check"]));
        for snapshot in fixture.content_snapshots(&fixture.alpha, repository) {
            assert_eq!(snapshot["hostname"], "alpha");
            assert!(
                snapshot["tags"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("utsurigi-agent-hub")),
                "{repository} snapshot must carry the canonical framework tag: {snapshot}"
            );
        }
    }
    let shared = fixture.ids(&fixture.alpha, "shared");
    assert!(!shared.is_empty());
    fixture.ok(&fixture.alpha, &["setup"]);
    assert!(snapshots.is_subset(&fixture.ids(&fixture.alpha, "bot")));
    assert!(shared.is_subset(&fixture.ids(&fixture.alpha, "shared")));
    let destination = fixture.temp.path().join("recovery");
    success(&restore(
        &fixture,
        "bot",
        snapshots.first().unwrap(),
        &destination,
    ));
    payload_at(&destination);
    assert_no_plaintext(&PathBuf::from(fixture.alpha.repository("bot")));
    assert_no_plaintext(&PathBuf::from(fixture.alpha.repository("shared")));
    assert_eq!(
        fs::metadata(fixture.alpha.path("state"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0,
        "state must be private"
    );
}

#[test]
fn allowlist_default_secret_and_git_exclusions_and_explicit_exclusions_apply() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["data"]["exclude"] = json!(["notes/excluded.txt"]);
    fixture.alpha.save();
    let owned = fixture.alpha.path("owned");
    let excluded = [
        "not-selected.txt",
        "notes/.env",
        "notes/.git/config",
        "notes/excluded.txt",
        "notes/nested/.ssh/id_ed25519",
        "notes/nested/.env.local",
        "notes/nested/credentials.json",
        "notes/nested/private-key.pem",
        "notes/nested/models/model.safetensors",
        "notes/nested/indexes/chunks.bin",
        "notes/nested/logs/run.log",
        "notes/nested/cache.sqlite-wal",
        "notes/nested/cache.sqlite-shm",
    ];
    for path in excluded {
        write(owned.join(path), b"must remain local");
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    let destination = fixture.temp.path().join("selected-recovery");
    success(&restore(
        &fixture,
        "bot",
        snapshots.first().unwrap(),
        &destination,
    ));
    payload_at(&destination);
    for path in excluded {
        assert!(
            !destination.join(path).exists(),
            "backed up excluded {path}"
        );
        assert_eq!(fs::read(owned.join(path)).unwrap(), b"must remain local");
    }
}

#[test]
fn wrong_password_and_existing_restore_destinations_fail_without_changing_data() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    let id = snapshots.first().unwrap();
    let wrong = fixture.temp.path().join("wrong-password");
    let output = fixture.cli_env(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            id,
            "--destination",
            wrong.to_str().unwrap(),
        ],
        &[("TEST_HUB_ALPHA_PASSWORD", SHARED_PASSWORD)],
    );
    failure(&output);
    assert!(!wrong.join("notes/日本語.md").exists());
    let existing = fixture.temp.path().join("existing");
    write(existing.join("keep"), b"local edit");
    failure(&restore(&fixture, "bot", id, &existing));
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"local edit");
    assert!(!existing.join("notes").exists());
    let empty = fixture.temp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    failure(&restore(&fixture, "bot", id, &empty));
    failure(&restore(&fixture, "bot", id, &fixture.alpha.path("owned")));
    payload_at(&fixture.alpha.path("owned"));
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
}

#[test]
fn shared_recovery_includes_the_published_registry_and_knowledge() {
    let fixture = Fixture::new();
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"shared knowledge",
    );
    fixture.ok(&fixture.alpha, &["setup"]);
    let current = fixture.current_shared_id(&fixture.alpha);
    let destination = fixture.temp.path().join("shared-recovery");
    success(&restore(&fixture, "shared", &current, &destination));
    assert_eq!(
        fs::read(destination.join("knowledge.md")).unwrap(),
        b"shared knowledge"
    );
    assert!(destination.join("registry.yaml").is_file());
    let registry = fs::read_to_string(destination.join("registry.yaml")).unwrap();
    let config = fs::read_to_string(&fixture.alpha.file).unwrap();
    for secret in [SHARED_PASSWORD, BOT_PASSWORD, PEER_PASSWORD] {
        assert!(
            !registry.contains(secret),
            "published registry contains a resolved secret"
        );
        assert!(
            !config.contains(secret),
            "configuration contains a resolved secret"
        );
    }
}

#[test]
fn malformed_unknown_or_wrongly_typed_configuration_never_backs_up() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(tool.join("calls"), "").unwrap();
    let baseline = fixture.alpha.config.clone();
    let mut variants = vec!["version: [unterminated".to_owned()];
    for (root, field, value) in [
        ("", "unrecognized", json!(true)),
        ("bot", "unrecognized", json!(true)),
        ("repositories", "unrecognized", json!({})),
        (
            "encryption",
            "key_ref",
            json!("op://synthetic/obsolete/key"),
        ),
        ("sharing", "bot_data", json!("false")),
        ("data", "include", json!("notes")),
        ("data", "sqlite_snapshots", json!("notes/export.sqlite")),
        ("sync", "interval_seconds", json!(0)),
        ("sync", "start_time", json!("24:00")),
        ("", "version", json!(999)),
        ("", "timezone", json!("Invalid/Timezone")),
    ] {
        let mut config = baseline.clone();
        if root.is_empty() {
            config[field] = value;
        } else {
            config[root][field] = value;
        }
        variants.push(serde_json::to_string(&config).unwrap());
    }
    for variant in variants {
        fs::write(&fixture.alpha.file, &variant).unwrap();
        failure(&fixture.cli(&fixture.alpha, &["validate"]));
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
        payload_at(&fixture.alpha.path("owned"));
    }
}

#[test]
fn escaping_allowlists_and_overlapping_roots_fail_before_backup() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(tool.join("calls"), "").unwrap();
    let baseline = fixture.alpha.config.clone();
    let outside = fixture.temp.path().join("outside");
    write(&outside, b"not owned");
    for include in [
        json!(["../outside"]),
        json!([outside]),
        json!(["notes/../../outside"]),
    ] {
        fixture.alpha.config = baseline.clone();
        fixture.alpha.config["data"]["include"] = include;
        fixture.alpha.save();
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
    }
    for (path, target) in [
        ("state", fixture.alpha.path("owned")),
        ("imports", fixture.alpha.path("owned").join("notes")),
        ("shared", fixture.alpha.path("owned")),
        ("state", fixture.alpha.path("imports").join("nested")),
        ("owned", fixture.alpha.path("state").join("nested")),
    ] {
        fixture.alpha.config = baseline.clone();
        fixture.alpha.config["paths"][path] = json!(target);
        fixture.alpha.save();
        failure(&fixture.cli(&fixture.alpha, &["validate"]));
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
    }
    assert_eq!(fs::read(outside).unwrap(), b"not owned");
}

#[test]
fn selected_and_nested_out_of_root_symlinks_never_back_up() {
    for nested in [false, true] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        fixture.ok(&fixture.alpha, &["setup"]);
        fs::write(tool.join("calls"), "").unwrap();
        let outside = fixture.temp.path().join("outside");
        write(outside.join("secret"), b"out-of-root secret");
        let link = fixture
            .alpha
            .path("owned")
            .join(if nested { "notes/link" } else { "link" });
        symlink(&outside, &link).unwrap();
        if !nested {
            fixture.alpha.config["data"]["include"] = json!(["link"]);
            fixture.alpha.save();
        }
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
        assert_eq!(
            fs::read(outside.join("secret")).unwrap(),
            b"out-of-root secret"
        );
    }
}

#[test]
fn selected_symlinks_are_refused_even_when_their_targets_are_inside_owned_data() {
    for nested in [false, true] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        fixture.ok(&fixture.alpha, &["setup"]);
        fs::write(tool.join("calls"), "").unwrap();
        let target = fixture.alpha.path("owned").join("targets");
        write(target.join("keep.md"), b"owned target bytes");
        let link = fixture
            .alpha
            .path("owned")
            .join(if nested { "notes/link" } else { "link" });
        symlink(&target, link).unwrap();
        if !nested {
            fixture.alpha.config["data"]["include"] = json!(["link"]);
            fixture.alpha.save();
        }
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
        assert_eq!(
            fs::read(target.join("keep.md")).unwrap(),
            b"owned target bytes"
        );
    }
}

#[test]
fn relative_data_paths_resolve_against_the_configuration_directory() {
    let mut fixture = Fixture::new();
    for name in ["owned", "shared", "imports", "state"] {
        fixture.alpha.config["paths"][name] = json!(name);
    }
    fixture.alpha.save();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    let destination = fixture.temp.path().join("relative-recovery");
    success(&restore(
        &fixture,
        "bot",
        snapshots.first().unwrap(),
        &destination,
    ));
    payload_at(&destination);
    assert!(!fixture.temp.path().join("owned").exists());
}

#[test]
fn ordinary_sync_never_initializes_missing_repositories() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert!(
        !calls
            .lines()
            .any(|call| ["init", "forget", "prune", "unlock"].contains(&call))
    );
    assert!(
        !Path::new(fixture.alpha.repository("shared"))
            .join("config")
            .exists()
    );
    assert!(
        !Path::new(fixture.alpha.repository("bot"))
            .join("config")
            .exists()
    );
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn status_readiness_does_not_require_connecting_to_a_repository() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fs::write(tool.join("mode"), "offline").unwrap();
    let status = fixture.status(&fixture.alpha);
    assert_eq!(status["ready"], true);
    assert_eq!(status["last_success"], Value::Null);
    assert_eq!(status["anchor"], Value::Null);
    assert_eq!(status["next_due"], Value::Null);
    assert_eq!(status["active_shared"], Value::Null);
    assert_no_writes(&tool);
}

#[test]
fn setup_persists_local_0300_and_manual_sync_and_restarts_preserve_the_schedule() {
    let mut fixture = Fixture::new();
    // Keep the scheduled no-op away from the 03:00 boundary on any test run.
    let timezone = if chrono::Utc::now().hour() == 2 {
        "Asia/Tokyo"
    } else {
        "UTC"
    };
    fixture.alpha.config["timezone"] = json!(timezone);
    fixture.alpha.save();
    let before = chrono::Utc::now().timestamp();
    fixture.ok(&fixture.alpha, &["setup"]);
    let after = chrono::Utc::now().timestamp();
    let initial = fixture.status(&fixture.alpha);
    assert_eq!(initial["ready"], true);
    assert_eq!(initial["last_success"], Value::Null);
    let (anchor, next_due) = schedule_fields(&initial);
    let utc_seconds = if timezone == "UTC" { 10_800 } else { 64_800 };
    assert_eq!(anchor.rem_euclid(86_400), utc_seconds);
    assert!(anchor >= before && anchor <= after + 86_400);
    assert_eq!(next_due, anchor);
    assert_eq!(
        schedule_fields(&fixture.status(&fixture.alpha)),
        (anchor, next_due)
    );
    fixture.ok(&fixture.alpha, &["setup"]);
    assert_eq!(
        schedule_fields(&fixture.status(&fixture.alpha)),
        (anchor, next_due)
    );
    let before_manual = chrono::Utc::now().timestamp();
    fixture.ok(&fixture.alpha, &["sync"]);
    let after_manual = chrono::Utc::now().timestamp();
    let manual = fixture.status(&fixture.alpha);
    assert_eq!(schedule_fields(&manual), (anchor, next_due));
    let last_success = status_timestamp(&manual["last_success"]);
    assert!((before_manual..=after_manual).contains(&last_success));
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    assert!(next_due > chrono::Utc::now().timestamp() + 2);
    fixture.ok(&fixture.alpha, &["sync", "--scheduled"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    assert_eq!(fixture.status(&fixture.alpha), manual);
}

#[test]
fn failed_sync_preserves_schedule_last_success_snapshots_and_local_edits() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let previous = fixture.status(&fixture.alpha);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    write(
        fixture.alpha.path("owned").join("notes/日本語.md"),
        b"unsynchronized edit",
    );
    fs::write(tool.join("mode"), "network").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_eq!(
        progress(&fixture.status(&fixture.alpha)),
        progress(&previous)
    );
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    assert_eq!(
        fs::read(fixture.alpha.path("owned").join("notes/日本語.md")).unwrap(),
        b"unsynchronized edit"
    );
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "backup").count(), 1);
    assert!(
        !calls
            .lines()
            .any(|call| ["forget", "prune", "unlock"].contains(&call))
    );
}

#[test]
fn exit_3_is_not_success_and_an_ambiguous_write_is_not_blindly_replayed() {
    for mode in ["partial", "ambiguous"] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        fixture.alpha.config["sync"]["history_retention_seconds"] = json!(0);
        fixture.alpha.save();
        fixture.ok(&fixture.alpha, &["setup"]);
        fixture.ok(&fixture.alpha, &["sync"]);
        let previous = fixture.status(&fixture.alpha);
        let old = fixture.ids(&fixture.alpha, "bot");
        write(
            fixture.alpha.path("owned").join("notes/new.md"),
            b"new local work",
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
        fs::write(tool.join("mode"), mode).unwrap();
        fs::write(tool.join("calls"), "").unwrap();
        let output = fixture.cli(&fixture.alpha, &["sync"]);
        failure(&output);
        assert!(old.is_subset(&fixture.ids(&fixture.alpha, "bot")));
        assert_eq!(
            fs::read(fixture.alpha.path("owned").join("notes/new.md")).unwrap(),
            b"new local work"
        );
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|call| *call == "backup").count(), 1);
        if mode == "partial" {
            assert_eq!(
                progress(&fixture.status(&fixture.alpha)),
                progress(&previous)
            );
        }
        assert!(
            !calls
                .lines()
                .any(|call| ["forget", "prune", "unlock"].contains(&call))
        );
        if mode == "partial" {
            let incomplete_history = fixture.ids(&fixture.alpha, "bot");
            assert!(incomplete_history.len() > old.len());
            fs::write(tool.join("mode"), "pass").unwrap();
            fixture.ok(&fixture.alpha, &["prune"]);
            assert_eq!(fixture.ids(&fixture.alpha, "bot"), incomplete_history);
        }
    }
}

#[test]
fn authentication_failure_is_visible_and_does_not_replay_backup() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    fs::write(tool.join("mode"), "auth").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "backup").count(), 1);
}

#[test]
fn authorized_peer_pull_is_separate_from_owned_data_and_requires_fresh_destination() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["setup"]);
    let shared = fixture.ids(&fixture.alpha, "shared");
    let destination = beta.path("imports").join("alpha-copy");
    fixture.ok(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fixture.ids(&fixture.alpha, "shared"),
        shared,
        "pull must not publish shared state"
    );
    payload_at(&destination);
    payload_at(&beta.path("owned"));
    write(destination.join("local-edit"), b"keep import edit");
    failure(&fixture.cli(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    ));
    assert_eq!(
        fs::read(destination.join("local-edit")).unwrap(),
        b"keep import edit"
    );
    failure(&fixture.cli(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            beta.path("owned").to_str().unwrap(),
        ],
    ));
    let owned_child = beta.path("owned").join("peer-data");
    failure(&fixture.cli(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            owned_child.to_str().unwrap(),
        ],
    ));
    assert!(!owned_child.exists());
    let unknown = beta.path("imports").join("unknown");
    failure(&fixture.cli(
        &beta,
        &[
            "pull",
            "unknown-bot",
            "--destination",
            unknown.to_str().unwrap(),
        ],
    ));
    assert!(!unknown.exists());
}

#[test]
fn private_peer_data_is_readable_with_its_password_and_preserves_the_source() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["setup"]);
    let destination = beta.path("imports").join("private-alpha");
    fixture.ok(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    payload_at(&destination);
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn peer_access_requires_the_shared_registry_password() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["setup"]);
    let destination = beta.path("imports").join("unauthorized");
    failure(&fixture.cli_env(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
        &[("TEST_HUB_SHARED_PASSWORD", PEER_PASSWORD)],
    ));
    assert!(!destination.exists());
}

#[test]
fn an_unregistered_bot_joins_but_must_copy_current_before_publication() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let beta = fixture.bot("beta", "beta-1");
    write(
        beta.path("shared").join("proposal.md"),
        b"unbased local proposal",
    );
    let before = fixture.shared_head(&fixture.alpha);
    failure(&fixture.cli(&beta, &["registry", "publish"]));
    assert!(!Path::new(beta.repository("bot")).join("config").exists());
    assert_eq!(fixture.shared_head(&fixture.alpha), before);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let joined = fixture.shared_head(&beta);
    failure(&fixture.cli(&beta, &["registry", "publish"]));
    assert_eq!(fixture.shared_head(&beta), joined);
    assert_eq!(
        fs::read(beta.path("shared").join("proposal.md")).unwrap(),
        b"unbased local proposal"
    );
    fixture.author_current(&beta);
    write(
        beta.path("shared").join("proposal.md"),
        b"rebased local proposal",
    );
    fixture.ok(&beta, &["registry", "publish"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(
        fs::read(fixture.active_shared(&fixture.alpha).join("proposal.md")).unwrap(),
        b"rebased local proposal"
    );
}

#[test]
fn explicit_takeover_supersedes_the_old_runtime_without_losing_its_work() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "bot");
    let successor = fixture.bot("alpha", "alpha-2");
    // Takeover acknowledges a runtime change; it does not supply an authoring base.
    fixture.registry(&successor, &[&successor]);
    failure(&fixture.cli(&successor, &["registry", "publish", "--takeover"]));
    copy_tree(
        &fixture.active_shared(&fixture.alpha),
        &successor.path("shared"),
    );
    fs::remove_file(successor.path("shared").join(HISTORY_FILE)).unwrap();
    fixture.registry(&successor, &[&successor]);
    failure(&fixture.cli(&successor, &["registry", "publish"]));
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    let accepted = fixture.shared_head(&successor);
    write(
        fixture.alpha.path("owned").join("notes/local-edit"),
        b"returning runtime work",
    );
    write(
        fixture.alpha.path("shared").join("old-runtime-proposal.md"),
        b"returning runtime proposal",
    );
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    failure(&fixture.cli(&fixture.alpha, &["registry", "publish"]));
    assert_eq!(fixture.shared_head(&successor), accepted);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), old);
    assert_eq!(
        fs::read(fixture.alpha.path("shared").join("old-runtime-proposal.md")).unwrap(),
        b"returning runtime proposal"
    );
    assert_eq!(
        fs::read(fixture.alpha.path("owned").join("notes/local-edit")).unwrap(),
        b"returning runtime work"
    );
}

#[test]
fn credential_holders_publish_from_current_and_sync_preserves_local_knowledge() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    let shared = fixture.ids(&fixture.alpha, "shared");
    write(
        fixture.alpha.path("shared").join("proposal.md"),
        b"unpublished proposal",
    );
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    assert_eq!(
        fs::read(fixture.alpha.path("shared").join("proposal.md")).unwrap(),
        b"unpublished proposal"
    );
    // Joining does not silently bless beta's unrelated authoring directory.
    failure(&fixture.cli(&beta, &["registry", "publish"]));
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    assert!(shared.is_subset(&fixture.ids(&fixture.alpha, "shared")));
    assert!(fixture.ids(&fixture.alpha, "shared").len() > shared.len());
    fixture.ok(&beta, &["sync"]);
    assert_eq!(
        fs::read(fixture.active_shared(&beta).join("proposal.md")).unwrap(),
        b"unpublished proposal"
    );
    assert!(!beta.path("shared").join("proposal.md").exists());
    fixture.author_current(&beta);
    write(beta.path("shared").join("proposal.md"), b"beta publication");
    fixture.ok(&beta, &["registry", "publish"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(
        fs::read(fixture.active_shared(&fixture.alpha).join("proposal.md")).unwrap(),
        b"beta publication"
    );
}

#[test]
fn private_repositories_cannot_reuse_shared_credentials_or_repository_locations() {
    for reuse_repository in [false, true] {
        let mut fixture = Fixture::new();
        fixture.alpha.config["sharing"]["bot_data"] = json!(false);
        if reuse_repository {
            fixture.alpha.config["repositories"]["bot"]["url"] =
                fixture.alpha.config["repositories"]["shared"]["url"].clone();
        }
        fixture.alpha.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha]);
        let output = if reuse_repository {
            fixture.cli(&fixture.alpha, &["setup"])
        } else {
            fixture.cli_env(
                &fixture.alpha,
                &["setup"],
                &[("TEST_HUB_ALPHA_PASSWORD", SHARED_PASSWORD)],
            )
        };
        failure(&output);
        payload_at(&fixture.alpha.path("owned"));
    }
}

#[test]
fn making_previously_shared_data_private_requires_a_new_repository_and_master_key() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let shared = fixture.ids(&fixture.alpha, "shared");
    let owned = fixture.ids(&fixture.alpha, "bot");
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    failure(&fixture.cli(&fixture.alpha, &["registry", "publish"]));
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), owned);
}

#[test]
fn failed_peer_restore_never_publishes_a_current_import_or_touches_local_edits() {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let tool = Fixture::wrapper(&mut beta);
    fixture.ok(&beta, &["setup"]);
    let previous = beta.path("imports").join("previous");
    write(previous.join("local-edit"), b"unsynchronized import edit");
    fs::write(tool.join("mode"), "restore-failure").unwrap();
    let destination = beta.path("imports").join("new-copy");
    failure(&fixture.cli(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    ));
    assert!(!destination.exists(), "failed import became current");
    assert_eq!(
        fs::read(previous.join("local-edit")).unwrap(),
        b"unsynchronized import edit"
    );
    payload_at(&beta.path("owned"));
    assert!(
        !fs::read_to_string(tool.join("calls"))
            .unwrap()
            .lines()
            .any(|call| call == "unlock")
    );
}

#[test]
fn explicit_prune_preserves_shared_roots_and_other_bots_data() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let alpha = fixture.ids(&fixture.alpha, "bot");
    let beta_ids = fixture.ids(&beta, "bot");
    let shared = fixture.ids(&fixture.alpha, "shared");
    fixture.ok(&beta, &["prune"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), alpha);
    assert_eq!(fixture.ids(&beta, "bot"), beta_ids);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    fixture.ok(&fixture.alpha, &["prune"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), alpha);
    assert_eq!(fixture.ids(&beta, "bot"), beta_ids);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
}

#[test]
fn explicit_prune_deletes_successful_predecessors_only_in_the_callers_repository() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sync"]["history_retention_seconds"] = json!(0);
    fixture.alpha.save();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["sync"]["history_retention_seconds"] = json!(0);
    beta.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let old_alpha = fixture.ids(&fixture.alpha, "bot");
    let old_beta = fixture.ids(&beta, "bot");
    let shared = fixture.ids(&fixture.alpha, "shared");
    std::thread::sleep(std::time::Duration::from_secs(1));
    write(
        fixture.alpha.path("owned").join("notes/second.md"),
        b"alpha successor",
    );
    write(
        beta.path("owned").join("notes/second.md"),
        b"beta successor",
    );
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&beta, &["sync"]);
    let all_alpha = fixture.ids(&fixture.alpha, "bot");
    let all_beta = fixture.ids(&beta, "bot");
    assert!(old_alpha.is_subset(&all_alpha));
    assert!(old_beta.is_subset(&all_beta));
    let current_alpha = all_alpha
        .difference(&old_alpha)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let current_beta = all_beta
        .difference(&old_beta)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(current_alpha.len(), 1);
    assert_eq!(current_beta.len(), 1);
    fixture.ok(&fixture.alpha, &["prune"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), current_alpha);
    assert_eq!(fixture.ids(&beta, "bot"), all_beta);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    fixture.ok(&beta, &["prune"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), current_alpha);
    assert_eq!(fixture.ids(&beta, "bot"), current_beta);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
}

#[test]
fn r2_budget_holds_without_a_valid_private_bounded_attestation() {
    for case in [
        "missing",
        "expired",
        "invalid-expiry",
        "malformed",
        "scope",
        "size",
        "public",
        "valid",
    ] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let local = common::Bot {
            file: fixture.alpha.file.clone(),
            config: fixture.alpha.config.clone(),
        };
        fs::write(tool.join("repo-shared"), local.repository("shared")).unwrap();
        fs::write(tool.join("repo-bot"), local.repository("bot")).unwrap();
        fixture.alpha.config["storage"]["provider"] = json!("r2");
        let shared = R2_SHARED;
        let bot = R2_BOT;
        fixture.alpha.config["repositories"]["shared"]["url"] = json!(shared);
        fixture.alpha.config["repositories"]["bot"]["url"] = json!(bot);
        let attestation = fixture.temp.path().join("attestation.json");
        let mut value = json!({
            "expires_at": "2999-01-01T00:00:00Z",
            "repositories": [shared, bot],
            "max_source_bytes": 1_000_000
        });
        fs::write(&attestation, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(&attestation, fs::Permissions::from_mode(0o600)).unwrap();
        fixture.alpha.config["budget"]["attestation_file"] = json!(attestation);
        fixture.alpha.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha]);
        fixture.fake_r2_head(&fixture.alpha, local.repository("shared"));
        let credentials = [
            ("AWS_ACCESS_KEY_ID", "synthetic-access-key"),
            ("AWS_SECRET_ACCESS_KEY", BOT_PASSWORD),
            ("AWS_SESSION_TOKEN", PEER_PASSWORD),
        ];
        success(&fixture.cli_env(&fixture.alpha, &["setup"], &credentials));
        success(&fixture.cli_env(&fixture.alpha, &["sync"], &credentials));
        let prior = fixture.ids(&local, "bot");
        let prior_status = fixture.status(&fixture.alpha);
        if case == "missing" {
            fixture.alpha.config["budget"]["attestation_file"] = Value::Null;
        } else {
            match case {
                "expired" => value["expires_at"] = json!("2000-01-01T00:00:00Z"),
                "invalid-expiry" => value["expires_at"] = json!("not a timestamp"),
                "scope" => value["repositories"] = json!(["s3:https://example.invalid/other"]),
                "size" => value["max_source_bytes"] = json!(1),
                _ => {}
            }
            fs::write(
                &attestation,
                if case == "malformed" {
                    b"not JSON".to_vec()
                } else {
                    serde_json::to_vec(&value).unwrap()
                },
            )
            .unwrap();
            fs::set_permissions(
                &attestation,
                fs::Permissions::from_mode(if case == "public" { 0o644 } else { 0o600 }),
            )
            .unwrap();
        }
        fixture.alpha.save();
        fs::write(tool.join("calls"), "").unwrap();
        fs::write(tool.join("boundary"), "").unwrap();
        let output = fixture.cli_env(&fixture.alpha, &["sync"], &credentials);
        if case == "valid" {
            success(&output);
            let calls = fs::read_to_string(tool.join("calls")).unwrap();
            assert_eq!(calls.lines().filter(|call| *call == "backup").count(), 1);
            assert!(prior.is_subset(&fixture.ids(&local, "bot")));
            let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
            let call = boundary
                .split("__CALL__\n")
                .find(|call| call.lines().any(|line| line == "arg:backup"))
                .unwrap();
            for (name, value) in credentials {
                assert_eq!(child_env(call, name), Some(value));
            }
            assert_eq!(child_env(call, "RESTIC_PASSWORD"), Some(BOT_PASSWORD));
        } else {
            failure(&output);
            assert_no_writes(&tool);
            assert_eq!(fixture.ids(&local, "bot"), prior);
            let status = fixture.status(&fixture.alpha);
            assert_eq!(status["last_success"], prior_status["last_success"]);
            assert_eq!(schedule_fields(&status), schedule_fields(&prior_status));
        }
        payload_at(&fixture.alpha.path("owned"));
    }
}

#[test]
fn declared_quiescent_sqlite_export_remains_recoverable() {
    let mut fixture = Fixture::new();
    let database: &[u8] = include_bytes!("fixtures/quiescent.sqlite");
    write(
        fixture.alpha.path("owned").join("notes/export.sqlite"),
        database,
    );
    fixture.alpha.config["data"]["sqlite_snapshots"] = json!(["notes/export.sqlite"]);
    fixture.alpha.save();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    let destination = fixture.temp.path().join("sqlite-recovery");
    success(&restore(
        &fixture,
        "bot",
        snapshots.first().unwrap(),
        &destination,
    ));
    payload_at(&destination);
    assert_eq!(
        fs::read(destination.join("notes/export.sqlite")).unwrap(),
        database
    );
}

#[test]
fn undeclared_sqlite_format_files_are_refused_even_without_sqlite_extensions() {
    for name in ["notes/live.sqlite", "notes/archive.bin"] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        fixture.ok(&fixture.alpha, &["setup"]);
        fs::write(tool.join("calls"), "").unwrap();
        let database: &[u8] = include_bytes!("fixtures/quiescent.sqlite");
        let source = fixture.alpha.path("owned").join(name);
        write(&source, database);
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
        assert_eq!(fs::read(&source).unwrap(), database);
        assert!(fixture.ids(&fixture.alpha, "bot").is_empty());
    }
}

#[test]
fn declaring_a_sqlite_export_does_not_authorize_wal_or_shm_sidecars() {
    for suffix in ["-wal", "-shm"] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        fixture.ok(&fixture.alpha, &["setup"]);
        fs::write(tool.join("calls"), "").unwrap();
        let database: &[u8] = include_bytes!("fixtures/quiescent.sqlite");
        let source = fixture.alpha.path("owned").join("notes/export.sqlite");
        write(&source, database);
        let sidecar = fixture
            .alpha
            .path("owned")
            .join(format!("notes/export.sqlite{suffix}"));
        write(&sidecar, b"unsettled sidecar");
        fixture.alpha.config["data"]["sqlite_snapshots"] = json!(["notes/export.sqlite"]);
        fixture.alpha.save();
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_no_writes(&tool);
        assert_eq!(fs::read(&source).unwrap(), database);
        assert_eq!(fs::read(&sidecar).unwrap(), b"unsettled sidecar");
    }
}

#[test]
fn shared_imports_publish_a_complete_pointer_and_preserve_both_authoring_trees() {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    let tool = Fixture::wrapper(&mut beta);
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"version one",
    );
    write(
        beta.path("shared").join("local-proposal.md"),
        b"local authoring edit",
    );
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    fixture.ok(&beta, &["sync"]);
    let first = fixture.active_shared(&beta);
    assert_eq!(
        fs::read(first.join("knowledge.md")).unwrap(),
        b"version one"
    );
    assert!(!beta.path("shared").join("knowledge.md").exists());
    fs::write(tool.join("calls"), "").unwrap();
    fixture.ok(&beta, &["sync"]);
    let repeated = fixture.active_shared(&beta);
    assert_eq!(repeated, first, "unchanged snapshot must reuse its version");
    assert_eq!(
        fs::read_to_string(tool.join("calls"))
            .unwrap()
            .lines()
            .filter(|call| *call == "restore")
            .count(),
        0,
        "after registration, repeated syncs of the unchanged head must reuse its cached version"
    );
    assert_eq!(
        fs::read(repeated.join("knowledge.md")).unwrap(),
        b"version one"
    );
    assert_eq!(
        fs::read(first.join("knowledge.md")).unwrap(),
        b"version one"
    );
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"version two",
    );
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    fixture.ok(&beta, &["sync"]);
    let second = fixture.active_shared(&beta);
    assert_ne!(second, first);
    assert_eq!(
        fs::read(second.join("knowledge.md")).unwrap(),
        b"version two"
    );
    assert_eq!(
        fs::read(first.join("knowledge.md")).unwrap(),
        b"version one"
    );

    let versions = second.parent().unwrap();
    assert_eq!(versions.file_name().unwrap(), "versions");
    let current = versions.parent().unwrap().join("CURRENT");
    let pointer = fs::read(&current).unwrap();

    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"version three",
    );
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let third_id = fixture.current_shared_id(&fixture.alpha);
    assert_ne!(third_id, first.file_name().unwrap().to_str().unwrap());
    assert_ne!(third_id, second.file_name().unwrap().to_str().unwrap());
    // A crash before pointer publication can leave an unreferenced version directory.
    write(versions.join(&third_id).join("partial"), b"unfinished");
    fs::write(tool.join("mode"), "restore-failure").unwrap();
    failure(&fixture.cli(&beta, &["sync"]));
    assert_eq!(fixture.active_shared(&beta), second);
    assert_eq!(fs::read(&current).unwrap(), pointer);
    assert_eq!(
        fs::read(second.join("knowledge.md")).unwrap(),
        b"version two"
    );
    assert_eq!(
        fs::read(first.join("knowledge.md")).unwrap(),
        b"version one"
    );
    assert_eq!(
        fs::read(beta.path("shared").join("local-proposal.md")).unwrap(),
        b"local authoring edit"
    );
    assert!(!beta.path("shared").join("knowledge.md").exists());
    assert_eq!(
        fs::read(fixture.alpha.path("shared").join("knowledge.md")).unwrap(),
        b"version three"
    );
    fs::write(tool.join("mode"), "pass").unwrap();
    // A killed process may leave staging data; it must never become a published version.
    write(versions.join(".staging-abandoned/partial"), b"unfinished");
    fixture.ok(&beta, &["sync"]);
    let third = fixture.active_shared(&beta);
    assert!(!third.join("partial").exists());
    assert_eq!(
        fs::read(third.join("knowledge.md")).unwrap(),
        b"version three"
    );
    assert!(
        !first.exists(),
        "only current and previous versions are retained"
    );
    assert!(second.is_dir());
    let retained: std::collections::BTreeSet<_> = fs::read_dir(versions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(retained, [second, third.clone()].into_iter().collect());
    let pointer: Value = serde_json::from_slice(&fs::read(current).unwrap()).unwrap();
    assert_eq!(
        pointer["current"],
        third.file_name().unwrap().to_str().unwrap()
    );
    assert!(third.file_name().unwrap().len() == 64);
    payload_at(&beta.path("owned"));
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn shared_cache_bots_reuse_one_version_and_observe_the_machine_pointer() {
    let mut fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["paths"]["imports"] = fixture.alpha.config["paths"]["imports"].clone();
    beta.save();
    let alpha_tool = Fixture::wrapper(&mut fixture.alpha);
    let beta_tool = Fixture::wrapper(&mut beta);
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    write(fixture.alpha.path("shared").join("knowledge.md"), b"one");
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    let first = fixture.active_shared(&fixture.alpha);
    fs::write(alpha_tool.join("calls"), "").unwrap();
    fs::write(beta_tool.join("calls"), "").unwrap();
    fixture.ok(&beta, &["sync"]);
    assert_eq!(fixture.active_shared(&beta), first);
    assert!(
        !fs::read_to_string(beta_tool.join("calls"))
            .unwrap()
            .lines()
            .any(|call| call == "restore")
    );
    let persisted: Value =
        serde_json::from_slice(&fs::read(fixture.alpha.path("state").join("status.json")).unwrap())
            .unwrap();
    assert!(
        persisted.get("active_shared").is_none(),
        "new Bot state must not own the shared pointer"
    );

    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    fixture.ok(&beta, &["sync"]);
    let second = fixture.active_shared(&beta);
    assert_ne!(second, first);
    assert_eq!(
        fixture.active_shared(&fixture.alpha),
        second,
        "status must follow another Bot's switch"
    );
    assert_eq!(fs::read(second.join("knowledge.md")).unwrap(), b"two");
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(fixture.active_shared(&fixture.alpha), second);
    assert_eq!(
        fs::read_to_string(alpha_tool.join("calls"))
            .unwrap()
            .lines()
            .filter(|call| *call == "restore")
            .count(),
        0
    );
}

#[test]
fn shared_cache_status_stays_read_only_while_a_reader_holds_the_lock() {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["paths"]["imports"] = fixture.alpha.config["paths"]["imports"].clone();
    beta.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    let first = fixture.active_shared(&fixture.alpha);
    assert_eq!(first.parent().unwrap().file_name().unwrap(), "versions");
    let root = first.parent().unwrap().parent().unwrap();
    let pointer = fs::read(root.join("CURRENT")).unwrap();
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"pending",
    );
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let lock = fs::OpenOptions::new()
        .write(true)
        .open(root.join("operation.lock"))
        .unwrap();
    lock.try_lock_shared().unwrap();
    lock.set_permissions(fs::Permissions::from_mode(0o640))
        .unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(fixture.active_shared(&fixture.alpha), first);
    assert_eq!(fixture.active_shared(&beta), first);
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), pointer);
    // The lock owner may adjust permissions; status must preserve them.
    fs::set_permissions(root, fs::Permissions::from_mode(0o750)).unwrap();
    lock.set_permissions(fs::Permissions::from_mode(0o640))
        .unwrap();
    fixture.ok(&beta, &["status"]);
    assert_eq!(
        fs::metadata(root).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o640);
    drop(lock);
    fixture.ok(&beta, &["sync"]);
    assert_ne!(fixture.active_shared(&beta), first);
}

#[test]
fn shared_cache_hits_still_validate_history_registry_and_current_runtime() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    let active = fixture.active_shared(&fixture.alpha);
    let history = active.join("registry-history.json");
    let original = fs::read(&history).unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    fs::write(&history, b"{}").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_no_writes(&tool);
    fs::write(&history, original).unwrap();
    fixture.alpha.config["bot"]["runtime_id"] = json!("superseded-runtime");
    fixture.alpha.save();
    let output = fixture.cli(&fixture.alpha, &["sync"]);
    failure(&output);
    assert!(text(&output).contains("superseded"));
    assert_no_writes(&tool);
    fixture.alpha.config["bot"]["runtime_id"] = json!("alpha-1");
    fixture.alpha.save();
    let registry = active.join("registry.yaml");
    let original = fs::read(&registry).unwrap();
    fs::write(&registry, b"{}").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    let mut reserved_registry: Value = serde_json::from_slice(&original).unwrap();
    let mut peer = fixture.bot("beta", "beta-1").entry();
    let mut invalid_password_names = Vec::new();
    for name in [
        "HTTP_PROXY",
        "RESTIC_CACERT",
        "RESTIC_TLS_CLIENT_CERT",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        peer["password_env"] = json!(name);
        reserved_registry["bots"] = json!([fixture.alpha.entry(), peer]);
        fs::write(&registry, serde_json::to_vec(&reserved_registry).unwrap()).unwrap();
        let output = fixture.cli(&fixture.alpha, &["sync"]);
        failure(&output);
        if !text(&output).contains("Invalid or duplicate registry identity/repository") {
            invalid_password_names.push(name);
        }
        assert_no_writes(&tool);
    }
    assert!(
        invalid_password_names.is_empty(),
        "active registry must reject reserved password names {invalid_password_names:?}"
    );
    fs::write(&registry, original).unwrap();
    assert_no_writes(&tool);
    assert!(
        !fs::read_to_string(tool.join("calls"))
            .unwrap()
            .lines()
            .any(|call| call == "restore"),
        "cache hits must revalidate without restoring"
    );
    fixture.ok(&fixture.alpha, &["sync"]);
    // Losing a referenced directory must remain repairable without changing CURRENT.
    let pointer = active.parent().unwrap().parent().unwrap().join("CURRENT");
    let original = fs::read(&pointer).unwrap();
    fs::remove_dir_all(&active).unwrap();
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(fixture.active_shared(&fixture.alpha), active);
    assert_eq!(fs::read(pointer).unwrap(), original);
}

#[test]
fn shared_cache_hubs_are_isolated_with_one_machine_import_root() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let first = fixture.active_shared(&fixture.alpha);
    let mut other = fixture.bot("beta", "other-1");
    other.config["hub"] = json!("other-hub");
    other.config["paths"]["imports"] = fixture.alpha.config["paths"]["imports"].clone();
    other.config["repositories"]["shared"]["url"] =
        json!(fixture.temp.path().join("repositories/other-shared"));
    other.save();
    fixture.registry(&other, &[&other]);
    fixture.ok(&other, &["setup"]);
    let second = fixture.active_shared(&other);
    assert_ne!(second.parent().unwrap(), first.parent().unwrap());
    assert_eq!(fixture.active_shared(&fixture.alpha), first);
    assert!(second.is_absolute());
}

#[test]
fn shared_cache_rejects_symlinked_pointer_and_cached_files_without_touching_targets() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    let active = fixture.active_shared(&fixture.alpha);
    assert_eq!(active.parent().unwrap().file_name().unwrap(), "versions");
    let pointer = active.parent().unwrap().parent().unwrap().join("CURRENT");
    let original = fs::read(&pointer).unwrap();
    let outside = fixture.temp.path().join("outside");
    fs::write(&outside, &original).unwrap();
    fs::remove_file(&pointer).unwrap();
    symlink(&outside, &pointer).unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["status"]));
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_no_writes(&tool);
    assert_eq!(fs::read(&outside).unwrap(), original);
    fs::remove_file(&pointer).unwrap();
    fs::write(&pointer, original).unwrap();
    symlink(&outside, active.join("knowledge.md")).unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_no_writes(&tool);
}

#[test]
fn shared_cache_status_actions_keep_ready_as_configuration_readiness() {
    let fixture = Fixture::new();
    let status = |bot: &Bot| -> Value {
        serde_json::from_slice(&fixture.ok(bot, &["status"]).stdout).unwrap()
    };
    let initial = status(&fixture.alpha);
    assert_eq!(initial["ready"], true);
    assert_eq!(initial["next_action"], "setup");
    assert!(
        initial["remediation"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    );
    assert!(!fixture.alpha.path("state").exists());
    assert!(!fixture.alpha.path("imports").exists());
    fixture.ok(&fixture.alpha, &["setup"]);
    let active = fixture.active_shared(&fixture.alpha);
    let path = fixture.alpha.path("state").join("status.json");
    let mut persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // Legacy Bot state remains readable, but cannot replace the machine pointer.
    persisted["active_shared"] = json!(fixture.alpha.path("owned"));
    persisted["next_due"] = json!(chrono::Utc::now().timestamp() + 10000);
    fs::write(&path, persisted.to_string()).unwrap();
    let waiting = status(&fixture.alpha);
    assert_eq!(waiting["ready"], true);
    assert_eq!(waiting["next_action"], "wait");
    assert_eq!(waiting["remediation"], json!([]));
    assert_eq!(waiting["active_shared"], json!(active));
    persisted["next_due"] = json!(1);
    fs::write(&path, persisted.to_string()).unwrap();
    assert_eq!(status(&fixture.alpha)["next_action"], "sync");
    fs::remove_file(active.parent().unwrap().parent().unwrap().join("CURRENT")).unwrap();
    let legacy_only = status(&fixture.alpha);
    assert_eq!(legacy_only["ready"], true);
    assert_eq!(legacy_only["active_shared"], Value::Null);
    assert_eq!(legacy_only["next_action"], "sync");
    assert!(
        legacy_only["remediation"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                let message = item.as_str().unwrap().to_lowercase();
                message.contains("sync") && message.contains("migrat")
            })
    );
    // Migration is required even when the existing schedule is not due yet.
    persisted["next_due"] = json!(chrono::Utc::now().timestamp() + 10000);
    fs::write(&path, persisted.to_string()).unwrap();
    assert_eq!(status(&fixture.alpha)["next_action"], "sync");
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(status(&fixture.alpha)["next_action"], "wait");
    persisted["anchor"] = Value::Null;
    persisted["next_due"] = Value::Null;
    fs::write(&path, persisted.to_string()).unwrap();
    assert_eq!(status(&fixture.alpha)["next_action"], "setup");
    assert_eq!(status(&fixture.alpha)["active_shared"], json!(active));
    let mut incomplete = Bot {
        file: fixture.temp.path().join("incomplete.yaml"),
        config: fixture.alpha.config.clone(),
    };
    incomplete.config["timezone"] = Value::Null;
    incomplete.save();
    assert_eq!(status(&incomplete)["ready"], false);
    assert_eq!(status(&incomplete)["next_action"], "configure");
}

#[test]
fn shared_cache_validation_rejects_exact_r2_placeholder_components() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["storage"]["provider"] = json!("r2");
    fixture.alpha.config["repositories"]["bot"]["url"] =
        json!("s3:https://real.r2.cloudflarestorage.com/real-bucket/bot");
    for (url, valid) in [
        (
            "s3:https://ACCOUNT.r2.cloudflarestorage.com/real/shared",
            false,
        ),
        (
            "s3:https://real.r2.cloudflarestorage.com/BUCKET/shared",
            false,
        ),
        ("s3:https://ACCOUNT/BUCKET/shared", false),
        (
            "s3:https://real.r2.cloudflarestorage.com/real/ACCOUNT/shared",
            false,
        ),
        (
            "s3:https://myACCOUNT.r2.cloudflarestorage.com/myBUCKET/shared",
            true,
        ),
        (
            "s3:https://account.r2.cloudflarestorage.com/bucket/shared",
            true,
        ),
        (
            "s3:https://real.r2.cloudflarestorage.com/real/policy.ACCOUNT.md",
            true,
        ),
    ] {
        fixture.alpha.config["repositories"]["shared"]["url"] = json!(url);
        fixture.alpha.save();
        let output = fixture.cli(&fixture.alpha, &["validate"]);
        assert_eq!(output.status.success(), valid, "{url}: {}", text(&output));
        redacted(&output);
    }
    assert!(!fixture.alpha.path("state").exists());
}

#[test]
fn restic_children_receive_only_the_selected_password_and_safe_environment() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fs::write(tool.join("mode"), "tls-boundary").unwrap();
    dotenv(
        &fixture.alpha,
        "TEST_HUB_ENV_FILE_SECRET=synthetic-dotenv-child-secret\nRESTIC_CACERT=/synthetic/file-restic-ca.pem\nRESTIC_TLS_CLIENT_CERT=/synthetic/file-restic-client.pem\n",
    );
    let marker = fixture.temp.path().join("must-not-execute");
    let password_command = format!("touch {}", marker.display());
    let poison_repo = fixture.temp.path().join("wrong-repository");
    let mut overrides = vec![
        ("TEST_HUB_UNRELATED_SECRET", PEER_PASSWORD),
        ("RESTIC_PASSWORD", "synthetic-wrong-inherited-password"),
        ("RESTIC_PASSWORD_COMMAND", password_command.as_str()),
        ("RESTIC_REPOSITORY", poison_repo.to_str().unwrap()),
        ("AWS_ACCESS_KEY_ID", "synthetic-unneeded-local-access-key"),
        ("AWS_SECRET_ACCESS_KEY", PEER_PASSWORD),
        ("AWS_SESSION_TOKEN", SHARED_PASSWORD),
        (
            "RESTIC_REPOSITORY_FILE",
            "/synthetic/unwanted-repository-file",
        ),
        ("RESTIC_REST_PASSWORD", "synthetic-unrelated-rest-password"),
        ("RESTIC_CACERT", "/synthetic/process-restic-ca.pem"),
        ("RESTIC_UNKNOWN_CONTROL", "synthetic-unwanted-control"),
        ("LANG", "unwanted-locale"),
        ("LC_ALL", "unwanted-locale"),
        ("TZ", "Asia/Tokyo"),
        ("SYSTEMROOT", "/synthetic/system-root"),
        ("WINDIR", "/synthetic/windows"),
    ];
    overrides.extend(CONNECTION_ENV);
    success(&fixture.cli_env(&fixture.alpha, &["setup"], &overrides));
    success(&fixture.cli_env(&fixture.alpha, &["sync"], &overrides));
    let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut backups = 0;
    for call in boundary.split("__CALL__\n").skip(1) {
        for (name, value) in CONNECTION_ENV {
            assert_eq!(
                child_env(call, name),
                Some(value),
                "connection variable {name}"
            );
        }
        for (name, value) in [
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("TZ", "UTC"),
            ("SYSTEMROOT", "/synthetic/system-root"),
            ("WINDIR", "/synthetic/windows"),
        ] {
            assert_eq!(child_env(call, name), Some(value));
        }
        assert!(child_env(call, "PATH").is_some_and(|value| !value.is_empty()));
        let args: Vec<_> = call
            .lines()
            .filter_map(|line| line.strip_prefix("arg:"))
            .collect();
        if args
            .iter()
            .any(|arg| ["version", "--version", "-V"].contains(arg))
        {
            for name in ["RESTIC_CACERT", "RESTIC_TLS_CLIENT_CERT"] {
                assert!(
                    child_env(call, name).is_none(),
                    "version check inherited {name}"
                );
            }
            continue;
        }
        for (name, value) in [
            ("RESTIC_CACERT", "/synthetic/process-restic-ca.pem"),
            (
                "RESTIC_TLS_CLIENT_CERT",
                "/synthetic/file-restic-client.pem",
            ),
        ] {
            assert_eq!(
                child_env(call, name),
                Some(value),
                "storage connection {name}"
            );
        }
        let repository = option(&args, "--repo").expect("restic receives explicit --repo");
        let password = if repository == fixture.alpha.repository("shared") {
            SHARED_PASSWORD
        } else {
            assert_eq!(repository, fixture.alpha.repository("bot"));
            BOT_PASSWORD
        };
        assert_eq!(child_env(call, "RESTIC_PASSWORD"), Some(password));
        let cache = option(&args, "--cache-dir")
            .or_else(|| child_env(call, "RESTIC_CACHE_DIR"))
            .expect("restic receives its private cache path");
        let cache = Path::new(cache);
        assert!(
            cache.starts_with(fixture.temp.path()),
            "cache escaped the test's configured state/cache roots"
        );
        assert!(cache.ancestors().any(|path| {
            fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o077 == 0)
        }));
        for name in [
            "TEST_HUB_SHARED_PASSWORD",
            "TEST_HUB_ALPHA_PASSWORD",
            "TEST_HUB_BETA_PASSWORD",
            "TEST_HUB_UNRELATED_SECRET",
            "TEST_HUB_ENV_FILE_SECRET",
            "RESTIC_PASSWORD_COMMAND",
            "RESTIC_REPOSITORY",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "RESTIC_REPOSITORY_FILE",
            "RESTIC_REST_PASSWORD",
            "RESTIC_UNKNOWN_CONTROL",
        ] {
            assert!(child_env(call, name).is_none(), "child inherited {name}");
        }
        for argument in &args {
            assert!(!argument.contains(password), "password appeared in argv");
        }
        if args.contains(&"backup") {
            if args.contains(&"kind:bot") || args.contains(&"kind:shared-v2") {
                backups += 1;
                assert_eq!(option(&args, "--host"), Some("alpha"));
            } else {
                assert!(
                    args.contains(&"--stdin-from-command")
                        || args.contains(&"kind:migration-capture"),
                    "unexpected control upload: {args:?}"
                );
            }
        }
    }
    assert!(
        backups >= 2,
        "setup publishes shared knowledge and sync publishes owned data"
    );
    assert!(!marker.exists());
    assert!(!poison_repo.exists());
    assert_eq!(
        fs::metadata(tool.join("boundary"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
}

#[test]
fn connection_environment_reaches_r2_storage_with_only_scoped_credentials() {
    let (fixture, _, tool, _) = history_r2_fixture();
    let mut environment = CONNECTION_ENV.to_vec();
    environment.extend(HISTORY_R2_ENV);
    environment.extend([
        ("AWS_SESSION_TOKEN", PEER_PASSWORD),
        ("TEST_HUB_UNRELATED_SECRET", "synthetic-unrelated-secret"),
        ("RESTIC_UNKNOWN_CONTROL", "synthetic-unwanted-control"),
    ]);
    success(&fixture.cli_env(&fixture.alpha, &["setup"], &environment));
    let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut repositories = std::collections::BTreeSet::new();
    for call in boundary.split("__CALL__\n").skip(1) {
        for (name, value) in CONNECTION_ENV {
            assert_eq!(
                child_env(call, name),
                Some(value),
                "connection variable {name}"
            );
        }
        let args: Vec<_> = call
            .lines()
            .filter_map(|line| line.strip_prefix("arg:"))
            .collect();
        let Some(repository) = option(&args, "--repo") else {
            for name in [
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
            ] {
                assert!(
                    child_env(call, name).is_none(),
                    "version check inherited {name}"
                );
            }
            continue;
        };
        repositories.insert(repository);
        for (name, value) in HISTORY_R2_ENV
            .into_iter()
            .chain([("AWS_SESSION_TOKEN", PEER_PASSWORD)])
        {
            assert_eq!(child_env(call, name), Some(value));
        }
        let password = if repository == fixture.alpha.repository("shared") {
            SHARED_PASSWORD
        } else {
            assert_eq!(repository, fixture.alpha.repository("bot"));
            BOT_PASSWORD
        };
        assert_eq!(child_env(call, "RESTIC_PASSWORD"), Some(password));
        for name in [
            "TEST_HUB_SHARED_PASSWORD",
            "TEST_HUB_ALPHA_PASSWORD",
            "TEST_HUB_BETA_PASSWORD",
            "TEST_HUB_UNRELATED_SECRET",
            "RESTIC_UNKNOWN_CONTROL",
        ] {
            assert!(child_env(call, name).is_none(), "storage inherited {name}");
        }
    }
    assert_eq!(repositories.len(), 2);
}

#[test]
fn password_values_with_shell_syntax_are_literal_and_redacted() {
    let fixture = Fixture::new();
    let marker = fixture.temp.path().join("password-must-not-execute");
    let password = format!(
        "synthetic $(touch {}) ; `touch {}`",
        marker.display(),
        marker.display()
    );
    for command in ["setup", "sync"] {
        let output = fixture.cli_env(
            &fixture.alpha,
            &[command],
            &[("TEST_HUB_ALPHA_PASSWORD", &password)],
        );
        success(&output);
        assert!(!text(&output).contains(&password));
        assert!(!marker.exists());
    }
}

#[test]
fn host_secret_providers_and_ai_prose_are_policies_without_shell_evaluation() {
    let mut fixture = Fixture::new();
    let marker = fixture.temp.path().join("prose-must-not-execute");
    let prose = format!("Keep selected notes; $(touch {})", marker.display());
    fixture.alpha.config["bot"]["role"] = json!(prose);
    for policy in ["sync", "reference", "local"] {
        fixture.alpha.config["data"][policy] = json!(prose);
    }
    for provider in ["Proton Pass", "proton-pass", "another-host-service"] {
        fixture.alpha.config["secrets"]["provider"] = json!(provider);
        fixture.alpha.save();
        fixture.ok(&fixture.alpha, &["validate"]);
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    assert!(!marker.exists());
}

fn strict_template_validation(document: &str) -> std::process::Output {
    let temp = TempDir::new();
    let file = temp.path().join("config.yaml");
    fs::write(&file, document).unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(temp.path())
        .arg("--config")
        .arg(file)
        .args(["validate", "--template"])
        .output()
        .expect("utsurigi-agent-hub must execute")
}

#[test]
fn strict_config_accepts_typed_scalars_and_unresolved_template_nulls() {
    for (format, document) in [
        (
            "YAML",
            "version: 1\nlanguage: \"123\"\nbot: {id: null, platform: null, runtime_id: null}\nsharing: {bot_data: false}\nsync: {interval_seconds: 86400}\ntasks: {enabled: true}\n",
        ),
        (
            "JSON",
            r#"{"version":1,"language":"123","bot":{"id":null,"platform":null,"runtime_id":null},"sharing":{"bot_data":false},"sync":{"interval_seconds":86400},"tasks":{"enabled":true}}"#,
        ),
    ] {
        let output = strict_template_validation(document);
        eprintln!("{format} typed/null template control: {}", text(&output));
        success(&output);
    }
}

#[test]
fn strict_config_rejects_scalar_coercion_and_unknown_keys() {
    let mut accepted = Vec::new();
    for (case, yaml, json) in [
        (
            "quoted boolean",
            "sharing: {bot_data: \"false\"}\n",
            r#"{"sharing":{"bot_data":"false"}}"#,
        ),
        (
            "quoted integer",
            "sync: {interval_seconds: \"86400\"}\n",
            r#"{"sync":{"interval_seconds":"86400"}}"#,
        ),
        (
            "number in string field",
            "language: 123\n",
            r#"{"language":123}"#,
        ),
        (
            "unknown nested key",
            "sharing: {unexpected: true}\n",
            r#"{"sharing":{"unexpected":true}}"#,
        ),
        (
            "duplicate top-level key",
            "version: 1\nversion: 1\n",
            r#"{"version":1,"version":1}"#,
        ),
        (
            "duplicate nested key",
            "storage: {provider: r2, provider: local}\n",
            r#"{"storage":{"provider":"r2","provider":"local"}}"#,
        ),
    ] {
        for (format, document) in [("YAML", yaml), ("JSON", json)] {
            let output = strict_template_validation(document);
            redacted(&output);
            eprintln!(
                "{format} {case}: exit={:?}, {}",
                output.status.code(),
                text(&output)
            );
            if output.status.success() {
                accepted.push(format!("{format} {case}"));
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "invalid configuration accepted: {}",
        accepted.join(", ")
    );
}

fn strict_registry_fixture() -> (Fixture, PathBuf, Value) {
    let mut fixture = Fixture::new();
    // Keep coerced scalars semantically valid so identity checks cannot mask type errors.
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.config["bot"]["runtime_id"] = json!("123");
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    let registry = serde_json::from_slice(
        &fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap(),
    )
    .unwrap();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(tool.join("calls"), "").unwrap();
    (fixture, tool, registry)
}

fn strict_registry_documents(registry: &Value) -> [(&'static str, String); 2] {
    [
        (
            "YAML",
            format!(
                "version: {}\nhub: {}\nmax_bots: {}\nbots: {}\n",
                registry["version"], registry["hub"], registry["max_bots"], registry["bots"]
            ),
        ),
        ("JSON", registry.to_string()),
    ]
}

#[test]
fn strict_registry_accepts_typed_scalars_in_yaml_and_json() {
    let (fixture, tool, registry) = strict_registry_fixture();
    for (format, document) in strict_registry_documents(&registry) {
        fs::write(fixture.alpha.path("shared").join("registry.yaml"), document).unwrap();
        let output = fixture.cli(&fixture.alpha, &["registry", "publish"]);
        eprintln!("{format} typed registry control: {}", text(&output));
        success(&output);
    }
    assert!(
        fs::read_to_string(tool.join("calls"))
            .unwrap()
            .lines()
            .any(|operation| operation == "backup")
    );
}

#[test]
fn strict_registry_rejects_scalar_coercion_and_unknown_keys_before_writes() {
    let (fixture, tool, baseline) = strict_registry_fixture();
    let mut accepted = Vec::new();
    for (case, pointer, value) in [
        ("quoted boolean", "/bots/0/shared", json!("false")),
        ("quoted version", "/version", json!("2")),
        ("quoted cap", "/max_bots", json!("50")),
        ("number in string field", "/bots/0/runtime_id", json!(123)),
        ("unknown nested key", "/bots/0", {
            let mut entry = baseline["bots"][0].clone();
            entry["unexpected"] = json!(true);
            entry
        }),
    ] {
        let mut registry = baseline.clone();
        *registry.pointer_mut(pointer).unwrap() = value;
        for (format, document) in strict_registry_documents(&registry) {
            fs::write(fixture.alpha.path("shared").join("registry.yaml"), document).unwrap();
            let output = fixture.cli(&fixture.alpha, &["registry", "publish"]);
            redacted(&output);
            eprintln!(
                "{format} {case}: exit={:?}, {}",
                output.status.code(),
                text(&output)
            );
            if output.status.success() {
                accepted.push(format!("{format} {case}"));
            }
        }
    }
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    let wrote = calls
        .lines()
        .any(|operation| matches!(operation, "backup" | "init" | "forget" | "prune" | "unlock"));
    assert!(
        accepted.is_empty() && !wrote,
        "invalid registry accepted: {}; restic calls:\n{calls}",
        accepted.join(", ")
    );
}

fn registry_privacy_fixture() -> (Fixture, PathBuf) {
    let mut fixture = Fixture::new();
    let tool = fixture.temp.path().join("registry-privacy-restic");
    fs::create_dir(&tool).unwrap();
    symlink(
        fixture.alpha.config["encryption"]["executable"]
            .as_str()
            .unwrap(),
        tool.join("real-restic"),
    )
    .unwrap();
    for name in ["calls", "peer-reads", "identity-reads"] {
        let file = tool.join(name);
        fs::write(&file, "").unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(
        tool.join("shared-repository"),
        fixture.alpha.repository("shared"),
    )
    .unwrap();
    let executable = tool.join("restic");
    fs::write(
        &executable,
        r#"#!/bin/sh
set -eu
tool=$(dirname -- "$0")
repository=
target=
operation=unknown
while [ "$#" -gt 0 ]; do
    case "$1" in
        version) printf '%s\n' 'restic 0.19.1 compiled with synthetic fixture'; exit 0 ;;
        --repo) repository=$2; shift ;;
        --target) target=$2; shift ;;
        snapshots|restore|init|backup|cat|tag|forget|prune|unlock) operation=$1 ;;
    esac
    shift
done
printf '%s\n' "$operation" >> "$tool/calls"
if [ "$operation" = cat ]; then
    printf '%s\n' "$repository" >> "$tool/identity-reads"
fi
if [ "$operation" = cat ] && [ -f "$tool/repository-config-$(basename -- "$repository")" ]; then
    cat "$tool/repository-config-$(basename -- "$repository")"
    exit 0
fi
if [ "$repository" != "$(cat "$tool/shared-repository")" ]; then
    printf '%s\n' "$repository" >> "$tool/peer-reads"
    exit 79
fi
case "$operation" in
    cat) exec "$tool/real-restic" --no-cache --repo "$repository" cat config ;;
    snapshots)
        [ -f "$tool/published-registry.yaml" ] || exit 10
        printf '[{"id":"%s","time":"2026-10-30T00:00:00Z","hostname":"alpha","tags":["utsurigi-agent-hub","hub:default","kind:shared-v2","bot:alpha","runtime:alpha-1"]}]\n' "$(cat "$tool/snapshot-id")"
        ;;
    restore)
        mkdir -p "$target"
        cp -R "$tool/published-source/." "$target/"
        ;;
    *) exit 79 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.alpha.config["encryption"]["executable"] = json!(executable);
    fixture.alpha.save();
    (fixture, tool)
}

fn registry_privacy_stage_registry(fixture: &Fixture, tool: &Path, registry: &Path) {
    let published = fs::read(registry).unwrap();
    let mut shared: Value = serde_json::from_slice(&published).unwrap();
    shared["bots"]
        .as_array_mut()
        .unwrap()
        .retain(|bot| bot["shared"] == true);
    let changed_projection = shared != serde_json::from_slice::<Value>(&published).unwrap();
    let mut seed = Bot {
        file: tool.join("generation-config.yaml"),
        config: fixture.alpha.config.clone(),
    };
    seed.config["encryption"]["executable"] =
        json!(tool.join("real-restic").canonicalize().unwrap());
    for (key, directory) in [
        ("shared", "generation-source"),
        ("imports", "generation-imports"),
        ("state", "generation-state"),
    ] {
        seed.config["paths"][key] = json!(tool.join(directory));
    }
    write(
        seed.path("shared").join("registry.yaml"),
        shared.to_string(),
    );
    seed.save();
    fixture.ok(&seed, &["setup"]);
    let source = fixture.active_shared(&seed);
    copy_tree(&source, &tool.join("published-source"));
    // Preserve the real accepted generation and its authoring metadata for
    // publication controls; only the deliberately invalid pull projection below
    // needs a synthetic immutable ID and an explicitly advanced head.
    let mut head = fixture.shared_head(&seed);

    // A valid projection must reach peer access, so bad metadata cannot explain a privacy rejection.
    fs::write(
        tool.join("published-registry.yaml"),
        fs::read(source.join("registry.yaml")).unwrap(),
    )
    .unwrap();
    fs::write(tool.join("snapshot-id"), head["snapshot"].as_str().unwrap()).unwrap();
    let destination = fixture.temp.path().join("privacy-fixture-positive-control");
    let output = fixture.cli(
        &fixture.alpha,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    failure(&output);
    assert_eq!(
        output.status.code(),
        Some(79),
        "valid generated history must reach the synthetic peer boundary: {}",
        text(&output)
    );
    assert!(
        fs::read_to_string(tool.join("peer-reads"))
            .unwrap()
            .lines()
            .any(|repository| repository == fixture.alpha.repository("bot"))
    );
    assert_no_writes(tool);
    assert!(!destination.exists());
    copy_tree(&source, &fixture.alpha.path("shared"));
    fs::remove_file(fixture.alpha.path("shared").join(HISTORY_FILE)).unwrap();
    fs::write(registry, &published).unwrap();
    if changed_projection {
        let id = format!("{:x}", Sha256::digest(&published));
        fs::write(tool.join("snapshot-id"), &id).unwrap();
        fs::write(tool.join("published-source/registry.yaml"), &published).unwrap();
        head["revision"] = json!(head["revision"].as_u64().unwrap() + 1);
        write_shared_revision(
            &seed,
            &tool.join("published-source"),
            head["revision"].as_u64().unwrap(),
        );
        head["snapshot"] = json!(id);
        fs::write(
            Path::new(seed.repository("shared")).join("uah-head.json"),
            serde_json::to_vec(&head).unwrap(),
        )
        .unwrap();
    }
    for name in ["calls", "peer-reads", "identity-reads"] {
        fs::write(tool.join(name), "").unwrap();
    }
}

fn registry_privacy_peer(fixture: &Fixture, id: &str, shared: bool, password_env: &str) -> Bot {
    let mut peer = fixture.bot(id, &format!("{id}-1"));
    peer.config["sharing"]["bot_data"] = json!(shared);
    peer.config["repositories"]["bot"]["password_env"] = json!(password_env);
    peer.save();
    peer
}

fn registry_privacy_reuse_is_refused(command: &str) {
    let mut violations = Vec::new();
    for (case, shared, names, references) in [
        (
            "private Bots, repeated delivered name",
            [false, false],
            ["TEST_HUB_BETA_PASSWORD", "TEST_HUB_BETA_PASSWORD"],
            [None, None],
        ),
        (
            "private Bots, repeated absent name",
            [false, false],
            [
                "TEST_HUB_UNDELIVERED_PASSWORD",
                "TEST_HUB_UNDELIVERED_PASSWORD",
            ],
            [None, None],
        ),
        (
            "private Bots, distinct names delivering equal values",
            [false, false],
            ["TEST_HUB_BETA_PASSWORD", "TEST_HUB_GAMMA_PASSWORD"],
            [None, None],
        ),
        (
            "private then shared Bot, repeated absent name",
            [false, true],
            [
                "TEST_HUB_UNDELIVERED_PASSWORD",
                "TEST_HUB_UNDELIVERED_PASSWORD",
            ],
            [None, None],
        ),
        (
            "shared then private Bot, repeated absent name",
            [true, false],
            [
                "TEST_HUB_UNDELIVERED_PASSWORD",
                "TEST_HUB_UNDELIVERED_PASSWORD",
            ],
            [None, None],
        ),
        (
            "private Bots, distinct absent names and equal references",
            [false, false],
            ["TEST_HUB_UNDELIVERED_BETA", "TEST_HUB_UNDELIVERED_GAMMA"],
            [Some("op://synthetic/reused/password"); 2],
        ),
        (
            "private then shared Bot, distinct absent names and equal references",
            [false, true],
            ["TEST_HUB_UNDELIVERED_BETA", "TEST_HUB_UNDELIVERED_GAMMA"],
            [Some("op://synthetic/reused/password"); 2],
        ),
        (
            "shared then private Bot, distinct absent names and equal references",
            [true, false],
            ["TEST_HUB_UNDELIVERED_BETA", "TEST_HUB_UNDELIVERED_GAMMA"],
            [Some("op://synthetic/reused/password"); 2],
        ),
        (
            "private peer, distinct absent name and shared-knowledge reference",
            [false, true],
            ["TEST_HUB_UNDELIVERED_BETA", "TEST_HUB_UNDELIVERED_GAMMA"],
            [Some("op://synthetic/shared/password"), None],
        ),
    ] {
        let (fixture, tool) = registry_privacy_fixture();
        let registry = fixture.alpha.path("shared").join("registry.yaml");
        if command == "publish" {
            registry_privacy_stage_registry(&fixture, &tool, &registry);
        }
        let mut beta = registry_privacy_peer(&fixture, "beta", shared[0], names[0]);
        let mut gamma = registry_privacy_peer(&fixture, "gamma", shared[1], names[1]);
        for (peer, reference) in [(&mut beta, references[0]), (&mut gamma, references[1])] {
            if let Some(reference) = reference {
                peer.config["repositories"]["bot"]["password_ref"] = json!(reference);
                peer.save();
            }
        }
        let delta = registry_privacy_peer(&fixture, "delta", true, "TEST_HUB_ALPHA_PASSWORD");
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma, &delta]);
        if command == "pull" {
            registry_privacy_stage_registry(&fixture, &tool, &registry);
        }
        let authored = fs::read(&registry).unwrap();
        let previous = fixture.status(&fixture.alpha);
        let destination = fixture.alpha.path("imports").join("peer-copy");
        let args = match command {
            "setup" => vec!["setup"],
            "publish" => vec!["registry", "publish"],
            "pull" => vec![
                "pull",
                "delta",
                "--destination",
                destination.to_str().unwrap(),
            ],
            _ => unreachable!(),
        };
        let output = fixture.cli_env(
            &fixture.alpha,
            &args,
            &[("TEST_HUB_GAMMA_PASSWORD", PEER_PASSWORD)],
        );
        redacted(&output);
        assert!(
            text(&output).contains("Repositories with different access policies require"),
            "{command}/{case} must reject credential access, not fixture metadata: {}",
            text(&output)
        );
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        if command == "pull" {
            assert_eq!(
                calls
                    .lines()
                    .filter(|operation| *operation == "restore")
                    .count(),
                1,
                "{command}/{case} must restore the changed registry before credential rejection"
            );
        }
        let peer_reads = fs::read_to_string(tool.join("peer-reads")).unwrap();
        let wrote = calls.lines().any(|operation| {
            matches!(
                operation,
                "init" | "backup" | "tag" | "forget" | "prune" | "unlock"
            )
        });
        eprintln!(
            "{command}: {case}: exit={:?}; writes={wrote}; peer_read={}; calls={:?}; {}",
            output.status.code(),
            !peer_reads.is_empty(),
            calls.lines().collect::<Vec<_>>(),
            text(&output)
        );
        if output.status.success() || wrote || !peer_reads.is_empty() {
            violations.push(case);
        }
        assert_eq!(fs::read(&registry).unwrap(), authored);
        assert_eq!(fixture.status(&fixture.alpha), previous);
        assert!(!destination.exists());
        for bot in [&fixture.alpha, &beta, &gamma, &delta] {
            payload_at(&bot.path("owned"));
        }
    }
    assert!(
        violations.is_empty(),
        "{command} did not refuse credential reuse before repository writes/peer reads: {}",
        violations.join("; ")
    );
}

#[test]
fn registry_privacy_setup_refuses_reuse_before_repository_writes() {
    registry_privacy_reuse_is_refused("setup");
}

#[test]
fn registry_privacy_publication_refuses_reuse_before_repository_writes() {
    registry_privacy_reuse_is_refused("publish");
}

#[test]
fn registry_privacy_pull_refuses_reuse_before_peer_reads() {
    registry_privacy_reuse_is_refused("pull");
}

#[test]
fn registry_privacy_control_private_peers_cannot_reuse_shared_knowledge_credentials() {
    for same_name in [false, true] {
        let (fixture, tool) = registry_privacy_fixture();
        let name = if same_name {
            "TEST_HUB_SHARED_PASSWORD"
        } else {
            "TEST_HUB_BETA_PASSWORD"
        };
        let beta = registry_privacy_peer(&fixture, "beta", false, name);
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
        let output = fixture.cli_env(
            &fixture.alpha,
            &["setup"],
            &[("TEST_HUB_BETA_PASSWORD", SHARED_PASSWORD)],
        );
        failure(&output);
        assert_no_writes(&tool);
        assert!(
            fs::read_to_string(tool.join("peer-reads"))
                .unwrap()
                .is_empty()
        );
        payload_at(&fixture.alpha.path("owned"));
    }
}

#[test]
fn registry_privacy_control_separate_private_credentials_work_without_absent_peer_secrets() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.config["repositories"]["bot"]["password_ref"] = Value::Null;
    fixture.alpha.save();
    let mut beta = registry_privacy_peer(&fixture, "beta", false, "TEST_HUB_BETA_PASSWORD");
    let mut gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_UNDELIVERED_GAMMA");
    let mut delta = registry_privacy_peer(&fixture, "delta", false, "TEST_HUB_UNDELIVERED_DELTA");
    beta.config["repositories"]["bot"]["password_ref"] = Value::Null;
    beta.save();
    for peer in [&mut gamma, &mut delta] {
        peer.config["repositories"]["bot"]["password_ref"] = json!("");
        peer.save();
    }
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma, &delta]);
    for bot in [&fixture.alpha, &beta] {
        fixture.ok(bot, &["setup"]);
        fixture.ok(bot, &["sync"]);
        assert_eq!(fixture.ids(bot, "bot").len(), 1);
    }
    for (bot, other_password) in [(&fixture.alpha, PEER_PASSWORD), (&beta, BOT_PASSWORD)] {
        failure(&registry_privacy_restic(
            &fixture,
            bot.repository("bot"),
            other_password,
            &["snapshots", "--json"],
        ));
    }
    let destination = beta.path("imports").join("private-alpha");
    fixture.ok(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    payload_at(&destination);
    payload_at(&fixture.alpha.path("owned"));
    payload_at(&beta.path("owned"));
}

#[test]
fn registry_privacy_control_shared_repositories_can_reuse_password_names_and_values() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["repositories"]["bot"]["password_env"] = json!("TEST_HUB_SHARED_PASSWORD");
    fixture.alpha.config["repositories"]["bot"]["password_ref"] =
        json!("op://synthetic/shared/password");
    fixture.alpha.save();
    let mut beta = registry_privacy_peer(&fixture, "beta", true, "TEST_HUB_SHARED_PASSWORD");
    let mut gamma = registry_privacy_peer(&fixture, "gamma", true, "TEST_HUB_BETA_PASSWORD");
    for peer in [&mut beta, &mut gamma] {
        peer.config["repositories"]["bot"]["password_ref"] =
            json!("op://synthetic/shared/password");
        peer.save();
    }
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
    let env = [("TEST_HUB_BETA_PASSWORD", SHARED_PASSWORD)];
    success(&fixture.cli_env(&fixture.alpha, &["setup"], &env));
    success(&fixture.cli_env(&fixture.alpha, &["sync"], &env));
    for peer in [&beta, &gamma] {
        success(&fixture.cli_env(peer, &["setup"], &env));
        let destination = peer.path("imports").join("shared-alpha");
        success(&fixture.cli_env(
            peer,
            &[
                "pull",
                "alpha",
                "--destination",
                destination.to_str().unwrap(),
            ],
            &env,
        ));
        payload_at(&destination);
    }
}

const REGISTRY_PRIVATE_PASSWORD: &str = "synthetic-GAMMA-secret-6d81";

fn registry_privacy_redacted(output: &std::process::Output) {
    redacted(output);
    assert!(
        !text(output).contains(REGISTRY_PRIVATE_PASSWORD),
        "output exposed a synthetic private credential"
    );
}

fn registry_privacy_restic(
    fixture: &Fixture,
    repository: &str,
    password: &str,
    args: &[&str],
) -> std::process::Output {
    let output = std::process::Command::new(
        fixture.alpha.config["encryption"]["executable"]
            .as_str()
            .unwrap(),
    )
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("TMPDIR", fixture.temp.path())
    .env("RESTIC_PASSWORD", password)
    .current_dir(fixture.temp.path())
    .args(["--no-cache", "--repo", repository])
    .args(args)
    .output()
    .unwrap();
    registry_privacy_redacted(&output);
    output
}

fn registry_privacy_transition_fixture(already_private: bool) -> (Fixture, Bot, Bot) {
    let fixture = Fixture::new();
    let beta = registry_privacy_peer(&fixture, "beta", true, "TEST_HUB_BETA_PASSWORD");
    let gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_GAMMA_PASSWORD");
    if already_private {
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
    } else {
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    }
    for bot in [&fixture.alpha, &beta] {
        let output = fixture.cli_env(
            bot,
            &["setup"],
            &[("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)],
        );
        registry_privacy_redacted(&output);
        success(&output);
    }
    // Public setup may record a newly initialized master ID even when its row
    // was predeclared. Author privacy proposals from that accepted history.
    registry_privacy_author_current(
        &fixture,
        &[("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)],
    );
    (fixture, beta, gamma)
}

fn registry_privacy_author_current(fixture: &Fixture, env: &[(&str, &str)]) {
    let output = fixture.cli_env(&fixture.alpha, &["setup"], env);
    registry_privacy_redacted(&output);
    success(&output);
    fixture.author_current(&fixture.alpha);
}

fn registry_privacy_assert_current_base(fixture: &Fixture) {
    let head = fixture.shared_head(&fixture.alpha);
    let marker: Value = serde_json::from_slice(
        &fs::read(fixture.alpha.path("shared").join("shared-revision.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        marker,
        json!({
            "version": 2,
            "hub": head["hub"],
            "repository_sha256": format!("{:x}", Sha256::digest(fixture.alpha.repository("shared").as_bytes())),
            "revision": head["revision"],
        }),
        "privacy rejection must start from a valid current authoring base"
    );
}

fn registry_privacy_publication_is_refused(
    fixture: &Fixture,
    env: &[(&str, &str)],
    case: &str,
) -> bool {
    registry_privacy_assert_current_base(fixture);
    let registry = fixture.alpha.path("shared").join("registry.yaml");
    let authored = fs::read(&registry).unwrap();
    let previous = fixture.status(&fixture.alpha);
    let before = fixture.ids(&fixture.alpha, "shared");
    assert!(!before.is_empty());
    let output = fixture.cli_env(&fixture.alpha, &["registry", "publish"], env);
    registry_privacy_redacted(&output);
    assert!(
        !text(&output).to_lowercase().contains("stale authoring"),
        "{case}: stale publication does not establish a privacy rejection"
    );
    let after = fixture.ids(&fixture.alpha, "shared");
    eprintln!(
        "{case}: exit={:?}; shared snapshots={} -> {}; {}",
        output.status.code(),
        before.len(),
        after.len(),
        text(&output)
    );
    assert_eq!(fs::read(registry).unwrap(), authored);
    assert_eq!(fixture.status(&fixture.alpha), previous);
    payload_at(&fixture.alpha.path("owned"));
    !output.status.success() && after == before
}

#[test]
fn registry_privacy_publication_refuses_prior_shared_urls_for_new_or_private_ids() {
    let mut violations = Vec::new();
    for already_private in [false, true] {
        let (fixture, mut beta, mut gamma) = registry_privacy_transition_fixture(already_private);
        let old_shared_repository = beta.repository("bot").to_owned();
        beta.config["repositories"]["bot"]["url"] =
            json!(fixture.temp.path().join("repositories/beta-fresh"));
        beta.config["repositories"]["bot"]["password_env"] =
            json!("TEST_HUB_ROTATED_BETA_PASSWORD");
        beta.config["repositories"]["bot"]["password_ref"] =
            json!("op://synthetic/beta-rotated/password");
        beta.save();
        gamma.config["repositories"]["bot"]["url"] = json!(old_shared_repository);
        gamma.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
        let case = if already_private {
            "already-private ID, prior shared URL"
        } else {
            "new private ID, prior shared URL"
        };
        if !registry_privacy_publication_is_refused(
            &fixture,
            &[
                ("TEST_HUB_BETA_PASSWORD", ""),
                ("TEST_HUB_GAMMA_PASSWORD", PEER_PASSWORD),
                ("TEST_HUB_ROTATED_BETA_PASSWORD", REGISTRY_PRIVATE_PASSWORD),
            ],
            case,
        ) {
            violations.push(case);
        }
        payload_at(&beta.path("owned"));
        payload_at(&gamma.path("owned"));
    }
    assert!(
        violations.is_empty(),
        "published prior shared repositories as private: {}",
        violations.join("; ")
    );
}

#[test]
fn registry_privacy_publication_refuses_prior_shared_passwords_at_different_urls() {
    let mut violations = Vec::new();
    for already_private in [false, true] {
        let (fixture, mut beta, mut gamma) = registry_privacy_transition_fixture(already_private);
        beta.config["repositories"]["bot"]["url"] =
            json!(fixture.temp.path().join("repositories/beta-fresh"));
        beta.config["repositories"]["bot"]["password_env"] =
            json!("TEST_HUB_ROTATED_BETA_PASSWORD");
        beta.config["repositories"]["bot"]["password_ref"] =
            json!("op://synthetic/beta-rotated/password");
        beta.save();
        gamma.config["repositories"]["bot"]["url"] =
            json!(fixture.temp.path().join("repositories/gamma-fresh"));
        gamma.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
        let case = if already_private {
            "already-private ID, prior shared password"
        } else {
            "new private ID, prior shared password"
        };
        if !registry_privacy_publication_is_refused(
            &fixture,
            &[
                ("TEST_HUB_GAMMA_PASSWORD", PEER_PASSWORD),
                ("TEST_HUB_ROTATED_BETA_PASSWORD", REGISTRY_PRIVATE_PASSWORD),
            ],
            case,
        ) {
            violations.push(case);
        }
    }
    assert!(
        violations.is_empty(),
        "published private repositories using prior shared passwords: {}",
        violations.join("; ")
    );
}

#[test]
fn registry_privacy_publication_refuses_cloned_shared_master_keys_after_password_rotation() {
    let (fixture, mut beta, gamma) = registry_privacy_transition_fixture(false);
    let original = fixture.restic(&beta, "bot", &["cat", "config"]);
    success(&original);
    let original: Value = serde_json::from_slice(&original.stdout).unwrap();
    success(
        &std::process::Command::new("cp")
            .args(["-a", "--", beta.repository("bot"), gamma.repository("bot")])
            .output()
            .unwrap(),
    );
    let password_file = fixture.temp.path().join("new-private-password");
    fs::write(&password_file, REGISTRY_PRIVATE_PASSWORD).unwrap();
    fs::set_permissions(&password_file, fs::Permissions::from_mode(0o600)).unwrap();
    success(&registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        PEER_PASSWORD,
        &[
            "key",
            "passwd",
            "--new-password-file",
            password_file.to_str().unwrap(),
        ],
    ));
    let clone = registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["cat", "config"],
    );
    success(&clone);
    let clone: Value = serde_json::from_slice(&clone.stdout).unwrap();
    assert_eq!(
        clone["id"], original["id"],
        "password rotation preserves the master key"
    );
    failure(&registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        PEER_PASSWORD,
        &["snapshots", "--json"],
    ));
    beta.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("repositories/beta-fresh"));
    beta.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
    assert!(
        registry_privacy_publication_is_refused(
            &fixture,
            &[("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)],
            "new private ID, cloned shared master key with a distinct password",
        ),
        "published a private clone of a prior shared master key"
    );
}

#[test]
fn registry_privacy_publication_refuses_uninspectable_prior_shared_repositories() {
    let mut violations = Vec::new();
    for malformed in [false, true] {
        let (fixture, tool) = registry_privacy_fixture();
        let beta = registry_privacy_peer(&fixture, "beta", true, "TEST_HUB_BETA_PASSWORD");
        let gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_GAMMA_PASSWORD");
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
        let registry = fixture.alpha.path("shared").join("registry.yaml");
        registry_privacy_stage_registry(&fixture, &tool, &registry);
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
        for (name, digit) in [("shared", "a"), ("alpha", "b"), ("gamma", "c")] {
            fs::write(
                tool.join(format!("repository-config-{name}")),
                json!({"id": digit.repeat(64)}).to_string(),
            )
            .unwrap();
        }
        if malformed {
            fs::write(
                tool.join("repository-config-beta"),
                r#"{"id":"not-a-repository-id"}"#,
            )
            .unwrap();
        }
        let output = fixture.cli_env(
            &fixture.alpha,
            &["registry", "publish"],
            &[("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)],
        );
        registry_privacy_redacted(&output);
        assert!(
            fs::read_to_string(tool.join("identity-reads"))
                .unwrap()
                .lines()
                .any(|repository| repository == beta.repository("bot")),
            "publication must reach the unavailable or malformed prior master"
        );
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        let wrote = calls.lines().any(|operation| {
            matches!(
                operation,
                "init" | "backup" | "tag" | "forget" | "prune" | "unlock"
            )
        });
        let case = if malformed {
            "malformed prior repository identity"
        } else {
            "unavailable prior repository"
        };
        eprintln!(
            "{case}: exit={:?}; writes={wrote}; calls={:?}; {}",
            output.status.code(),
            calls.lines().collect::<Vec<_>>(),
            text(&output)
        );
        if output.status.success() || wrote {
            violations.push(case);
        }
        payload_at(&fixture.alpha.path("owned"));
    }
    assert!(
        violations.is_empty(),
        "publication did not fail closed: {}",
        violations.join("; ")
    );
}

#[test]
fn registry_privacy_control_fresh_private_repositories_allow_joining_and_same_id_rotation() {
    for rotate_existing_id in [false, true] {
        let (fixture, mut beta, gamma) = registry_privacy_transition_fixture(false);
        let delta = registry_privacy_peer(&fixture, "delta", false, "TEST_HUB_UNDELIVERED_DELTA");
        let private = if rotate_existing_id {
            beta.config["sharing"]["bot_data"] = json!(false);
            beta.config["repositories"]["bot"]["url"] =
                json!(fixture.temp.path().join("repositories/beta-private-fresh"));
            beta.config["repositories"]["bot"]["password_env"] = json!("TEST_HUB_GAMMA_PASSWORD");
            beta.config["repositories"]["bot"]["password_ref"] =
                json!("op://synthetic/beta-private/password");
            beta.save();
            fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &delta]);
            &beta
        } else {
            fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma, &delta]);
            &gamma
        };
        let env = [("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)];
        let before = fixture.ids(&fixture.alpha, "shared");
        assert!(!Path::new(private.repository("bot")).exists());
        let output = fixture.cli_env(&fixture.alpha, &["registry", "publish"], &env);
        registry_privacy_redacted(&output);
        success(&output);
        assert_ne!(fixture.ids(&fixture.alpha, "shared"), before);
        assert!(!Path::new(private.repository("bot")).exists());
        for command in ["setup", "sync"] {
            let output = fixture.cli_env(private, &[command], &env);
            registry_privacy_redacted(&output);
            success(&output);
        }
        let snapshots = registry_privacy_restic(
            &fixture,
            private.repository("bot"),
            REGISTRY_PRIVATE_PASSWORD,
            &["snapshots", "--json"],
        );
        success(&snapshots);
        let snapshots: Vec<Value> = serde_json::from_slice(&snapshots.stdout).unwrap();
        assert_eq!(snapshots.len(), 1);
        failure(&registry_privacy_restic(
            &fixture,
            private.repository("bot"),
            PEER_PASSWORD,
            &["snapshots", "--json"],
        ));
        payload_at(&private.path("owned"));
        eprintln!(
            "fresh private repository control: same-ID rotation={rotate_existing_id}; publication, setup, sync and key separation succeeded"
        );
    }
}

#[test]
fn credential_holder_can_move_its_private_repository_without_rotating_credentials() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    let registry = fixture.alpha.path("shared").join("registry.yaml");
    for file in [&fixture.alpha.file, &registry] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let previous = Bot {
        file: fixture.temp.path().join("previous-owner.yaml"),
        config: fixture.alpha.config.clone(),
    };
    previous.save();
    fs::set_permissions(&previous.file, fs::Permissions::from_mode(0o600)).unwrap();
    let old_snapshots = fixture.ids(&previous, "bot");
    assert_eq!(old_snapshots.len(), 1);
    let old_key = fixture.restic(&previous, "bot", &["cat", "config"]);
    success(&old_key);
    let old_key: Value = serde_json::from_slice(&old_key.stdout).unwrap();
    let edit = fixture.alpha.path("owned").join("notes/local-edit");
    write(&edit, b"private owner work during repository move");
    fs::set_permissions(&edit, fs::Permissions::from_mode(0o600)).unwrap();
    fixture.alpha.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("repositories/alpha-moved"));
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);

    let mut outputs = Vec::new();
    for args in [&["registry", "publish"][..], &["setup"][..], &["sync"][..]] {
        if args == ["registry", "publish"] {
            fixture.author_current(&fixture.alpha);
            fixture.registry(&fixture.alpha, &[&fixture.alpha]);
        }
        let output = fixture.cli(&fixture.alpha, args);
        redacted(&output);
        eprintln!(
            "private owner move {args:?}: exit={:?}; {}",
            output.status.code(),
            text(&output)
        );
        outputs.push(output);
    }
    assert_eq!(fixture.ids(&previous, "bot"), old_snapshots);
    payload_at(&fixture.alpha.path("owned"));
    assert_eq!(
        fs::read(&edit).unwrap(),
        b"private owner work during repository move"
    );
    let recovery = fixture.temp.path().join("previous-owner-recovery");
    fixture.ok(
        &previous,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            old_snapshots.first().unwrap(),
            "--destination",
            recovery.to_str().unwrap(),
        ],
    );
    payload_at(&recovery);
    eprintln!("private owner move preserved old snapshots, recoverable bytes and local work");
    for output in outputs {
        success(&output);
    }
    let new_key = fixture.restic(&fixture.alpha, "bot", &["cat", "config"]);
    success(&new_key);
    let new_key: Value = serde_json::from_slice(&new_key.stdout).unwrap();
    assert_ne!(old_key["id"], new_key["id"]);
    let moved_snapshots = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(moved_snapshots.len(), 1);
    let recovery = fixture.temp.path().join("moved-owner-recovery");
    success(&restore(
        &fixture,
        "bot",
        moved_snapshots.first().unwrap(),
        &recovery,
    ));
    payload_at(&recovery);
    assert_eq!(
        fs::read(recovery.join("notes/local-edit")).unwrap(),
        b"private owner work during repository move"
    );
    let published: Value = serde_saphyr::from_str(
        &fs::read_to_string(fixture.active_shared(&fixture.alpha).join("registry.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(published["version"], 2);
    assert!(published.get("owner").is_none());
    assert_eq!(published["max_bots"], 50);
    assert_eq!(
        published["bots"][0],
        json!({
            "id": "alpha", "runtime_id": "alpha-1", "shared": false,
            "repository": fixture.alpha.repository("bot"),
            "password_env": "TEST_HUB_ALPHA_PASSWORD",
            "password_ref": "op://synthetic/alpha/password",
        })
    );
}

#[test]
fn another_credential_holder_can_move_its_private_repository_from_current() {
    let fixture = Fixture::new();
    let mut beta = registry_privacy_peer(&fixture, "beta", false, "TEST_HUB_BETA_PASSWORD");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let authored_registry = fixture.alpha.path("shared").join("registry.yaml");
    for file in [&fixture.alpha.file, &beta.file, &authored_registry] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let previous = Bot {
        file: fixture.temp.path().join("previous-beta.yaml"),
        config: beta.config.clone(),
    };
    previous.save();
    fs::set_permissions(&previous.file, fs::Permissions::from_mode(0o600)).unwrap();
    let owned = fixture.ids(&previous, "bot");
    let shared = fixture.ids(&fixture.alpha, "shared");
    write(
        beta.path("owned").join("notes/local-edit"),
        b"private beta work during repository move",
    );
    beta.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("repositories/beta-moved"));
    beta.save();
    fixture.author_current(&beta);
    fixture.registry(&beta, &[&fixture.alpha, &beta]);
    let registry = beta.path("shared").join("registry.yaml");
    for file in [&beta.file, &registry] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    for args in [&["registry", "publish"][..], &["setup"][..], &["sync"][..]] {
        if args == ["registry", "publish"] {
            fixture.author_current(&beta);
            fixture.registry(&beta, &[&fixture.alpha, &beta]);
        }
        fixture.ok(&beta, args);
    }
    assert!(Path::new(beta.repository("bot")).join("config").is_file());
    assert_eq!(fixture.ids(&previous, "bot"), owned);
    assert!(shared.is_subset(&fixture.ids(&fixture.alpha, "shared")));
    payload_at(&beta.path("owned"));
    assert_eq!(
        fs::read(beta.path("owned").join("notes/local-edit")).unwrap(),
        b"private beta work during repository move"
    );
    let current = fixture.active_shared(&beta);
    let published: Value =
        serde_saphyr::from_slice(&fs::read(current.join("registry.yaml")).unwrap()).unwrap();
    assert!(published.get("owner").is_none());
    assert!(
        published["bots"]
            .as_array()
            .unwrap()
            .contains(&beta.entry())
    );
    let recovery = fixture.temp.path().join("previous-beta-recovery");
    fixture.ok(
        &previous,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            owned.first().unwrap(),
            "--destination",
            recovery.to_str().unwrap(),
        ],
    );
    payload_at(&recovery);
}

#[test]
fn registry_owner_control_initialized_shared_peer_auth_failure_blocks_publication() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let registry = fixture.alpha.path("shared").join("registry.yaml");
    for file in [&fixture.alpha.file, &beta.file, &registry] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    registry_privacy_author_current(&fixture, &[]);
    let peer = fixture.ids(&beta, "bot");
    let shared = fixture.ids(&fixture.alpha, "shared");
    let auth = registry_privacy_restic(
        &fixture,
        beta.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["cat", "config"],
    );
    failure(&auth);
    assert_ne!(auth.status.code(), Some(10));
    let output = fixture.cli_env(
        &fixture.alpha,
        &["registry", "publish"],
        &[("TEST_HUB_BETA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)],
    );
    registry_privacy_redacted(&output);
    failure(&output);
    assert_eq!(fixture.ids(&beta, "bot"), peer);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    payload_at(&fixture.alpha.path("owned"));
    payload_at(&beta.path("owned"));
}

#[test]
fn registry_owner_control_missing_shared_history_still_forbids_url_and_password_reuse() {
    for reuse_url in [false, true] {
        let mut fixture = Fixture::new();
        fixture.alpha.config["sharing"]["bot_data"] = json!(false);
        fixture.alpha.save();
        let mut beta = fixture.bot("beta", "beta-1");
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
        let registry = fixture.alpha.path("shared").join("registry.yaml");
        for file in [&fixture.alpha.file, &beta.file, &registry] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
        }
        success(&fixture.cli_env(
            &fixture.alpha,
            &["setup"],
            &[("TEST_HUB_BETA_PASSWORD", "")],
        ));
        let missing = fixture.restic(&beta, "bot", &["cat", "config"]);
        failure(&missing);
        assert_eq!(missing.status.code(), Some(10), "{}", text(&missing));
        let prior_url = beta.repository("bot").to_owned();
        beta.config["repositories"]["bot"]["url"] =
            json!(fixture.temp.path().join("repositories/beta-fresh"));
        beta.config["repositories"]["bot"]["password_env"] = json!("TEST_HUB_SHARED_PASSWORD");
        beta.config["repositories"]["bot"]["password_ref"] =
            json!("op://synthetic/shared/password");
        beta.save();
        let mut gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_GAMMA_PASSWORD");
        if reuse_url {
            gamma.config["repositories"]["bot"]["url"] = json!(prior_url);
            gamma.save();
        }
        fs::set_permissions(&gamma.file, fs::Permissions::from_mode(0o600)).unwrap();
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta, &gamma]);
        let shared = fixture.ids(&fixture.alpha, "shared");
        let password = if reuse_url {
            REGISTRY_PRIVATE_PASSWORD
        } else {
            PEER_PASSWORD
        };
        let output = fixture.cli_env(
            &fixture.alpha,
            &["registry", "publish"],
            &[("TEST_HUB_GAMMA_PASSWORD", password)],
        );
        registry_privacy_redacted(&output);
        eprintln!(
            "missing shared history reuse_url={reuse_url}: exit={:?}; {}",
            output.status.code(),
            text(&output)
        );
        failure(&output);
        assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
        for bot in [&beta, &gamma] {
            assert!(!Path::new(bot.repository("bot")).exists());
            payload_at(&bot.path("owned"));
        }
        payload_at(&fixture.alpha.path("owned"));
    }
}

#[test]
fn registry_owner_control_missing_prior_shared_master_still_blocks_publication() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let registry = fixture.alpha.path("shared").join("registry.yaml");
    for file in [&fixture.alpha.file, &beta.file, &registry] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let snapshots = fixture.ids(&beta, "bot");
    let shared = fixture.ids(&fixture.alpha, "shared");
    let mut relocated = Bot {
        file: beta.file.clone(),
        config: beta.config.clone(),
    };
    relocated.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("repositories/relocated-beta"));
    fs::rename(beta.repository("bot"), relocated.repository("bot")).unwrap();
    let missing = fixture.restic(&beta, "bot", &["cat", "config"]);
    failure(&missing);
    assert_eq!(missing.status.code(), Some(10), "{}", text(&missing));
    let output = fixture.cli(&fixture.alpha, &["registry", "publish"]);
    failure(&output);
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
    assert_eq!(fixture.ids(&relocated, "bot"), snapshots);
    assert!(!Path::new(beta.repository("bot")).exists());
    payload_at(&fixture.alpha.path("owned"));
    payload_at(&beta.path("owned"));
}

#[test]
fn final_fix_prior_shared_identifiers_are_refused_without_delivered_passwords() {
    let mut violations = Vec::new();
    for field in ["password_ref", "password_env"] {
        let (mut fixture, beta, mut gamma) = registry_privacy_transition_fixture(false);
        gamma.config["repositories"]["bot"][field] =
            beta.config["repositories"]["bot"][field].clone();
        gamma.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &gamma]);
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let refused = registry_privacy_publication_is_refused(
            &fixture,
            &[
                ("TEST_HUB_BETA_PASSWORD", ""),
                ("TEST_HUB_GAMMA_PASSWORD", ""),
            ],
            field,
        );
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        if !refused || calls.lines().any(|call| matches!(call, "init" | "backup")) {
            violations.push(field);
        }
        assert!(!Path::new(gamma.repository("bot")).exists());
        payload_at(&gamma.path("owned"));
    }
    assert!(
        violations.is_empty(),
        "published retired shared identifiers with absent secrets: {violations:?}"
    );
}

#[test]
fn final_fix_pending_historical_shared_url_and_reference_reuse_is_refused() {
    let mut violations = Vec::new();
    for field in ["url", "password_ref"] {
        let (fixture, beta, mut gamma) = registry_privacy_transition_fixture(false);
        let historical = fixture.ids(&fixture.alpha, "shared");
        fixture.registry(&fixture.alpha, &[&fixture.alpha]);
        fixture.ok(&fixture.alpha, &["registry", "publish"]);
        assert!(historical.is_subset(&fixture.ids(&fixture.alpha, "shared")));
        gamma.config["repositories"]["bot"][field] =
            beta.config["repositories"]["bot"][field].clone();
        gamma.save();
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &gamma]);
        if !registry_privacy_publication_is_refused(
            &fixture,
            &[
                ("TEST_HUB_BETA_PASSWORD", ""),
                ("TEST_HUB_GAMMA_PASSWORD", ""),
            ],
            field,
        ) {
            violations.push(field);
        }
        assert!(historical.is_subset(&fixture.ids(&fixture.alpha, "shared")));
        if field != "url" {
            assert!(!Path::new(gamma.repository("bot")).exists());
        }
        payload_at(&gamma.path("owned"));
    }
    assert!(
        violations.is_empty(),
        "OPEN I2: published historical shared identifiers: {violations:?}"
    );
}

#[test]
fn final_fix_explicit_excluded_includes_fail_before_repository_initialization() {
    let mut violations = Vec::new();
    for (include, exclude) in [
        ("notes/auth.md", vec![]),
        ("notes/selected.txt", vec!["notes/selected.txt"]),
    ] {
        let mut fixture = Fixture::new();
        fixture.alpha.config["data"]["include"] = json!([include]);
        fixture.alpha.config["data"]["exclude"] = json!(exclude);
        fixture.alpha.save();
        write(
            fixture.alpha.path("owned").join(include),
            b"synthetic protected selection",
        );
        let tool = Fixture::wrapper(&mut fixture.alpha);
        let output = fixture.cli(&fixture.alpha, &["setup"]);
        redacted(&output);
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        eprintln!(
            "explicit include {include:?}: exit={:?}; calls={calls:?}; {}",
            output.status.code(),
            text(&output)
        );
        let initialized = ["bot", "shared"].iter().any(|name| {
            Path::new(fixture.alpha.repository(name))
                .join("config")
                .exists()
        });
        if output.status.success()
            || initialized
            || calls.lines().any(|call| matches!(call, "init" | "backup"))
        {
            violations.push(include);
        }
        assert_eq!(
            fs::read(fixture.alpha.path("owned").join(include)).unwrap(),
            b"synthetic protected selection"
        );
        assert!(!text(&output).contains("synthetic protected selection"));
    }
    assert!(
        violations.is_empty(),
        "silently accepted explicit excluded includes: {violations:?}"
    );
}

#[test]
fn final_fix_recursive_exclusions_are_named_safely_and_never_published() {
    let mut missing = Vec::new();
    for shared in [false, true] {
        let mut fixture = Fixture::new();
        fixture.alpha.config["data"]["exclude"] = json!(["notes/nested/selected.txt"]);
        fixture.alpha.save();
        let (root, paths, prefix) = if shared {
            (
                fixture.alpha.path("shared"),
                [
                    "knowledge/logs",
                    "policies/CrEdEnTiAlS.json",
                    "practices/token.\nFORGED\x1b[31m.txt",
                ],
                "Excluded shared path:",
            )
        } else {
            (
                fixture.alpha.path("owned"),
                [
                    "notes/nested/logs",
                    "notes/nested/selected.txt",
                    "notes/token.\nFORGED\x1b[31m.txt",
                ],
                "Excluded owned path:",
            )
        };
        let protected = b"synthetic-excluded-content-7e";
        write(root.join(paths[0]).join("hidden.txt"), protected);
        for path in &paths[1..] {
            write(root.join(path), protected);
        }
        write(
            root.join(if shared {
                "knowledge/visible.md"
            } else {
                "notes/visible.md"
            }),
            b"visible content",
        );
        for args in [
            vec!["setup"],
            if shared {
                vec!["registry", "publish"]
            } else {
                vec!["sync"]
            },
        ] {
            let output = fixture.ok(&fixture.alpha, &args);
            let output = text(&output);
            assert!(!output.contains(std::str::from_utf8(protected).unwrap()));
            assert!(
                !output.contains('\x1b') && !output.contains("\nFORGED"),
                "path injected terminal output: {output:?}"
            );
            for path in paths {
                let expected = format!("{prefix} {:?}", Path::new(path));
                let prefixed = format!("{prefix} {:?}", Path::new(".").join(path));
                if !output
                    .lines()
                    .any(|line| line == expected || line == prefixed)
                {
                    missing.push(format!("{args:?}: {expected}"));
                }
            }
            assert!(
                !output.contains("logs/hidden.txt"),
                "warning traversed an excluded subtree"
            );
        }
        let repository = if shared { "shared" } else { "bot" };
        let destination = fixture.temp.path().join("excluded-recovery");
        let snapshot = if shared {
            fixture.current_shared_id(&fixture.alpha)
        } else {
            fixture
                .ids(&fixture.alpha, repository)
                .first()
                .unwrap()
                .clone()
        };
        success(&restore(&fixture, repository, &snapshot, &destination));
        for path in paths {
            assert!(
                !destination.join(path).exists(),
                "published excluded path {path:?}"
            );
        }
        assert_eq!(
            fs::read(root.join(paths[0]).join("hidden.txt")).unwrap(),
            protected
        );
        for path in &paths[1..] {
            assert_eq!(fs::read(root.join(path)).unwrap(), protected);
        }
        assert_eq!(
            fs::read(destination.join(if shared {
                "knowledge/visible.md"
            } else {
                "notes/visible.md"
            }))
            .unwrap(),
            b"visible content"
        );
        payload_at(&fixture.alpha.path("owned"));
    }
    assert!(
        missing.is_empty(),
        "missing escaped exclusion warnings:\n{}",
        missing.join("\n")
    );
}

#[test]
fn final_fix_status_reads_last_atomic_state_while_operation_lock_is_held() {
    let fixture = Fixture::new();
    let root = fixture.alpha.path("state");
    fs::create_dir(&root).unwrap();
    let state = json!({"hub": "default", "last_success": 100, "anchor": 1, "next_due": 86401, "active_shared": null});
    write(root.join("status.next"), state.to_string());
    fs::rename(root.join("status.next"), root.join("status.json")).unwrap();
    write(root.join("unfinished-write"), b"{");
    let lock = fs::File::create(root.join("operation.lock")).unwrap();
    lock.try_lock().unwrap();
    lock.set_permissions(fs::Permissions::from_mode(0o640))
        .unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    let output = fixture.cli(&fixture.alpha, &["status"]);
    success(&output);
    let mut expected = state.clone();
    expected["last_success"] = json!("1970-01-01T00:01:40Z");
    expected["anchor"] = json!("1970-01-01T00:00:01Z");
    expected["next_due"] = json!("1970-01-02T00:00:01Z");
    expected["ready"] = json!(true);
    expected["joined"] = json!(false);
    expected["failure"] = Value::Null;
    expected["next_action"] = json!("setup");
    expected["remediation"] =
        json!(["Run setup to register with the shared hub and initialize the schedule"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        expected
    );
    assert_eq!(
        fs::read_to_string(root.join("status.json")).unwrap(),
        state.to_string()
    );
    assert_eq!(
        fs::metadata(root).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o640);
}

#[test]
fn status_formats_optional_seconds_without_writing_state() {
    let fixture = Fixture::new();
    let root = fixture.alpha.path("state");
    let file = root.join("status.json");
    for (seconds, expected) in [
        (None, Value::Null),
        (Some(-1_i64), json!("1969-12-31T23:59:59Z")),
        (Some(0), json!("1970-01-01T00:00:00Z")),
        (Some(-62_167_219_200), json!("0000-01-01T00:00:00Z")),
        (Some(253_402_300_799), json!("9999-12-31T23:59:59Z")),
    ] {
        let state = json!({
            "hub": "default", "last_success": seconds,
            "anchor": seconds, "next_due": seconds, "active_shared": null,
        });
        let bytes = serde_json::to_vec(&state).unwrap();
        write(&file, &bytes);
        let status = fixture.status(&fixture.alpha);
        for field in ["last_success", "anchor", "next_due"] {
            assert_eq!(status[field], expected, "{field} for {seconds:?}");
        }
        assert_eq!(status["active_shared"], Value::Null);
        assert_eq!(fs::read(&file).unwrap(), bytes);
        assert!(!root.join("operation.lock").exists());
        assert!(!fixture.alpha.path("imports").exists());
    }
}

#[test]
fn status_rejects_unrepresentable_times_without_writing_state() {
    let fixture = Fixture::new();
    let root = fixture.alpha.path("state");
    let file = root.join("status.json");
    for field in ["last_success", "anchor", "next_due"] {
        for seconds in [i64::MIN, i64::MAX, -62_167_219_201, 253_402_300_800] {
            let mut state = json!({
                "hub": "default", "last_success": 100,
                "anchor": 1, "next_due": 86401, "active_shared": null,
            });
            state[field] = json!(seconds);
            let bytes = serde_json::to_vec(&state).unwrap();
            write(&file, &bytes);
            let output = fixture.cli(&fixture.alpha, &["status"]);
            failure(&output);
            assert_eq!(output.status.code(), Some(1), "{field} for {seconds}");
            assert!(output.stdout.is_empty());
            let message = String::from_utf8(output.stderr).unwrap();
            assert!(message.contains("out of range"), "{message}");
            assert!(message.contains("RFC3339"), "{message}");
            assert_eq!(fs::read(&file).unwrap(), bytes);
            assert!(!root.join("operation.lock").exists());
            assert!(!fixture.alpha.path("imports").exists());
        }
    }
}

#[test]
fn final_fix_status_does_not_create_locks_or_change_directory_permissions() {
    let fixture = Fixture::new();
    let root = fixture.alpha.path("state");
    let absent = fixture.status(&fixture.alpha);
    assert_eq!(absent["last_success"], Value::Null);
    assert!(!root.exists());
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    assert_eq!(fixture.status(&fixture.alpha), absent);
    assert!(
        !root.join("operation.lock").exists()
            && fs::metadata(root).unwrap().permissions().mode() & 0o777 == 0o750,
        "status created a lock or chmodded its state directory"
    );
}

#[test]
fn final_fix_control_status_rejects_malformed_state_and_symlinked_paths() {
    let fixture = Fixture::new();
    let root = fixture.alpha.path("state");
    write(root.join("status.json"), b"{");
    let output = fixture.cli(&fixture.alpha, &["status"]);
    failure(&output);
    assert!(text(&output).contains("Invalid persisted state"));
    fs::remove_file(root.join("status.json")).unwrap();
    symlink(
        fixture.alpha.path("owned").join("notes/日本語.md"),
        root.join("status.json"),
    )
    .unwrap();
    failure(&fixture.cli(&fixture.alpha, &["status"]));
    payload_at(&fixture.alpha.path("owned"));
    fs::remove_file(root.join("status.json")).unwrap();
    fs::remove_file(root.join("operation.lock")).ok();
    fs::remove_dir(&root).unwrap();
    symlink(fixture.alpha.path("owned"), &root).unwrap();
    failure(&fixture.cli(&fixture.alpha, &["status"]));
    assert!(!fixture.alpha.path("owned").join("operation.lock").exists());
    payload_at(&fixture.alpha.path("owned"));
}

fn legacy_incomplete_shared(fixture: &Fixture) -> Value {
    // Seed the pre-v2 interrupted write directly. New setup's first backup is
    // its format fence, so injecting exit 3 there would test a different path.
    fixture.legacy_registry(&fixture.alpha, &[&fixture.alpha]);
    success(&fixture.restic(&fixture.alpha, "shared", &["init"]));
    success(&fixture.restic(
        &fixture.alpha,
        "shared",
        &[
            "backup",
            ".",
            "--host",
            "alpha",
            "--tag",
            "utsurigi-agent-hub",
            "--tag",
            "hub:default",
            "--tag",
            "kind:shared",
            "--tag",
            "bot:alpha",
            "--tag",
            "runtime:alpha-1",
        ],
    ));
    let incomplete = fixture.snapshots(&fixture.alpha, "shared");
    assert_eq!(incomplete.len(), 1);
    let tags = incomplete[0]["tags"].as_array().unwrap();
    for tag in [
        "utsurigi-agent-hub",
        "hub:default",
        "kind:shared",
        "bot:alpha",
        "runtime:alpha-1",
    ] {
        assert!(tags.contains(&json!(tag)));
    }
    assert!(
        !tags
            .iter()
            .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
    );
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    incomplete[0].clone()
}

#[test]
fn final_fix_first_incomplete_legacy_setup_can_retry_without_losing_history() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let incomplete = legacy_incomplete_shared(&fixture);
    assert_eq!(fixture.status(&fixture.alpha)["anchor"], Value::Null);
    fs::write(tool.join("mode"), "pass").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    fixture.ok(&fixture.alpha, &["setup"]);
    let after = fixture.snapshots(&fixture.alpha, "shared");
    assert!(
        after.contains(&incomplete),
        "retry changed the incomplete historical snapshot"
    );
    let current = fixture.current_shared_id(&fixture.alpha);
    assert_ne!(current, incomplete["id"].as_str().unwrap());
    assert_eq!(fixture.content_snapshots(&fixture.alpha, "shared").len(), 1);
    assert!(after.iter().any(|snapshot| {
        snapshot["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("uah2:kind:format-fence"))
    }));
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert_eq!(
        calls.lines().filter(|call| *call == "backup").count(),
        2,
        "retry creates one null-source fence and one accepted v2 state, without replaying a data upload"
    );
    assert!(
        !calls
            .lines()
            .any(|call| matches!(call, "forget" | "prune" | "unlock"))
    );
    let (anchor, next_due) = schedule_fields(&fixture.status(&fixture.alpha));
    assert_eq!(anchor, next_due);
    assert!(
        fixture
            .active_shared(&fixture.alpha)
            .join("registry.yaml")
            .is_file()
    );
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn final_fix_control_initial_setup_refuses_any_foreign_or_uncertain_history() {
    for case in [
        "foreign-bot",
        "superseded-runtime",
        "untagged",
        "changed-runtime-candidate",
    ] {
        let mut fixture = Fixture::new();
        let tool = Fixture::wrapper(&mut fixture.alpha);
        legacy_incomplete_shared(&fixture);
        if case == "changed-runtime-candidate" {
            let file = fixture.alpha.path("shared").join("registry.yaml");
            let mut registry: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
            registry["bots"][0]["runtime_id"] = json!("alpha-0");
            fs::write(file, registry.to_string()).unwrap();
        } else {
            let args = match case {
                "foreign-bot" => vec![
                    "backup",
                    ".",
                    "--host",
                    "beta",
                    "--tag",
                    "utsurigi-agent-hub",
                    "--tag",
                    "hub:default",
                    "--tag",
                    "kind:shared",
                    "--tag",
                    "bot:beta",
                    "--tag",
                    "runtime:beta-1",
                ],
                "superseded-runtime" => vec![
                    "backup",
                    ".",
                    "--host",
                    "alpha",
                    "--tag",
                    "utsurigi-agent-hub",
                    "--tag",
                    "hub:default",
                    "--tag",
                    "kind:shared",
                    "--tag",
                    "bot:alpha",
                    "--tag",
                    "runtime:alpha-0",
                ],
                _ => vec!["backup", ".", "--host", "alpha", "--tag", "hub:default"],
            };
            success(&fixture.restic(&fixture.alpha, "shared", &args));
        }
        let before = fixture.snapshots(&fixture.alpha, "shared");
        fs::write(tool.join("mode"), "pass").unwrap();
        fs::write(tool.join("calls"), "").unwrap();
        let output = fixture.cli(&fixture.alpha, &["setup"]);
        eprintln!(
            "incomplete recovery control {case}: exit={:?}; {}",
            output.status.code(),
            text(&output)
        );
        failure(&output);
        if case != "changed-runtime-candidate" {
            assert!(
                text(&output).contains("uncertain history"),
                "{}",
                text(&output)
            );
        }
        assert_eq!(fixture.snapshots(&fixture.alpha, "shared"), before);
        assert_no_writes(&tool);
        assert_eq!(fixture.status(&fixture.alpha)["anchor"], Value::Null);
        payload_at(&fixture.alpha.path("owned"));
    }
}

fn assert_restic_lock_wait(sync: Option<Value>, expected: &str) {
    let mut fixture = Fixture::new();
    if let Some(sync) = sync {
        fixture.alpha.config["sync"] = sync;
        fixture.alpha.save();
    }
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    // Shared v2 setup uses CAS; Bot-data sync exercises completion tagging.
    fixture.ok(&fixture.alpha, &["sync"]);
    let boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut completion_calls = 0;
    for call in boundary.split("__CALL__\n").skip(1) {
        let args: Vec<_> = call
            .lines()
            .filter_map(|line| line.strip_prefix("arg:"))
            .collect();
        if args.contains(&"version") {
            continue;
        }
        assert_eq!(
            option(&args, "--retry-lock"),
            Some(expected),
            "restic arguments: {args:?}"
        );
        if args.contains(&"tag") {
            completion_calls += 1;
        }
    }
    assert!(
        completion_calls > 0,
        "lock wait must reach completion tagging"
    );
}

#[test]
fn final_fix_control_explicit_zero_lock_wait_reaches_restic() {
    assert_restic_lock_wait(Some(json!({"retry_lock_seconds": 0})), "0s");
}

#[test]
fn final_fix_default_lock_wait_reaches_restic_as_120_seconds() {
    assert_restic_lock_wait(None, "120s");
}

#[test]
fn final_fix_public_template_lock_wait_reaches_restic_as_120_seconds() {
    let defaults: Value = serde_saphyr::from_str(include_str!("../config.yaml")).unwrap();
    assert_restic_lock_wait(Some(defaults["sync"].clone()), "120s");
}

#[test]
fn final_fix_control_due_scheduled_sync_advances_without_moving_the_anchor() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let file = fixture.alpha.path("state").join("status.json");
    let mut persisted: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let anchor = chrono::Utc::now().timestamp() - 3 * 86400 - 3600;
    persisted["anchor"] = json!(anchor);
    persisted["next_due"] = json!(anchor);
    fs::write(file, persisted.to_string()).unwrap();
    fixture.ok(&fixture.alpha, &["sync", "--scheduled"]);
    let status = fixture.status(&fixture.alpha);
    assert_eq!(schedule_fields(&status), (anchor, anchor + 4 * 86400));
    assert!(status_timestamp(&status["last_success"]) > anchor);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(snapshots.len(), 1);
    let skipped = fixture.ok(&fixture.alpha, &["sync", "--scheduled"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&skipped.stdout).unwrap(),
        json!({"skipped": true})
    );
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    assert_eq!(fixture.status(&fixture.alpha), status);
}

const HISTORY_FILE: &str = "registry-history.json";

fn history_retired_fixture() -> (Fixture, Bot) {
    let (mut fixture, beta, _) = registry_privacy_transition_fixture(false);
    fixture.ok(&beta, &["sync"]);
    fixture.alpha.config["sync"]["history_retention_seconds"] = json!(0);
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    for generation in 1..=2 {
        write(
            fixture.alpha.path("shared").join("knowledge/generation.md"),
            format!("publication {generation}"),
        );
        fixture.ok(&fixture.alpha, &["registry", "publish"]);
    }
    (fixture, beta)
}

fn history_restored_shared(fixture: &Fixture, name: &str) -> PathBuf {
    let current = fixture.current_shared_id(&fixture.alpha);
    let destination = fixture.temp.path().join(name);
    success(&restore(fixture, "shared", &current, &destination));
    destination
}

fn history_generated_bytes(root: &Path) -> Vec<u8> {
    let bytes = fs::read(root.join(HISTORY_FILE))
        .expect("complete shared snapshots must contain generated registry-history.json");
    assert!(
        serde_json::from_slice::<Value>(&bytes).is_ok(),
        "generated history must be JSON"
    );
    bytes
}

fn history_contains(bytes: &[u8], identifier: &str) -> bool {
    assert!(!identifier.is_empty());
    bytes
        .windows(identifier.len())
        .any(|window| window == identifier.as_bytes())
}

fn history_assert_encrypted(root: &Path, identifiers: &[&str]) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            history_assert_encrypted(&path, identifiers);
        } else {
            let bytes = fs::read(path).unwrap();
            for identifier in identifiers {
                assert!(
                    !history_contains(&bytes, identifier),
                    "restic storage exposed a plaintext shared identifier"
                );
            }
        }
    }
}

#[test]
fn history_retired_identifiers_are_refused_after_generations_prune_and_fresh_takeover() {
    let (mut fixture, beta) = history_retired_fixture();
    let retired = beta.entry();
    let old_peer = fixture.ids(&beta, "bot");
    let old_shared = fixture.ids(&fixture.alpha, "shared");
    assert!(fixture.content_snapshots(&fixture.alpha, "shared").len() >= 4);
    let mut tool = Fixture::wrapper(&mut fixture.alpha);
    let mut publisher = Bot {
        file: fixture.alpha.file.clone(),
        config: fixture.alpha.config.clone(),
    };
    let mut violations = Vec::new();
    for phase in ["generations", "pruned-fresh-state", "takeover"] {
        if phase == "pruned-fresh-state" {
            fixture.registry(&publisher, &[&publisher]);
            fixture.ok(&publisher, &["registry", "publish"]);
            fs::write(tool.join("calls"), "").unwrap();
            fixture.ok(&publisher, &["prune"]);
            let retained = fixture.ids(&fixture.alpha, "shared");
            assert!(
                old_shared.is_subset(&retained),
                "shared current/candidate/control/capture roots must survive prune"
            );
            let calls = fs::read_to_string(tool.join("calls")).unwrap();
            assert!(!calls.lines().any(|call| call == "unlock"));
            publisher.config["paths"]["state"] =
                json!(fixture.temp.path().join("fresh-owner-state"));
            publisher.config["paths"]["imports"] =
                json!(fixture.temp.path().join("fresh-owner-imports"));
            publisher.save();
            assert!(!publisher.path("state").exists());
            assert!(!publisher.path("imports").exists());
        } else if phase == "takeover" {
            publisher = fixture.bot("alpha", "alpha-history-successor");
            tool = Fixture::wrapper(&mut publisher);
            // Import as the still-current runtime, then copy its authoring base
            // before acknowledging the intended runtime handover.
            let runtime = publisher.config["bot"]["runtime_id"].clone();
            publisher.config["bot"]["runtime_id"] =
                fixture.alpha.config["bot"]["runtime_id"].clone();
            publisher.save();
            fixture.ok(&publisher, &["setup"]);
            fixture.author_current(&publisher);
            publisher.config["bot"]["runtime_id"] = runtime;
            publisher.save();
            fixture.registry(&publisher, &[&publisher]);
            fixture.ok(&publisher, &["registry", "publish", "--takeover"]);
            failure(&fixture.cli(&fixture.alpha, &["registry", "publish"]));
        }
        for (field, old_field) in [
            ("url", Some("repository")),
            ("password_ref", Some("password_ref")),
            ("password_env", Some("password_env")),
            ("delivered-value", None),
        ] {
            let mut gamma = registry_privacy_peer(
                &fixture,
                &format!("gamma-{phase}-{field}"),
                false,
                "TEST_HUB_HISTORY_PRIVATE_PASSWORD",
            );
            if let Some(old_field) = old_field {
                gamma.config["repositories"]["bot"][field] = retired[old_field].clone();
            }
            gamma.save();
            fixture.registry(&publisher, &[&publisher, &gamma]);
            let authored = fs::read(publisher.path("shared").join("registry.yaml")).unwrap();
            let before = fixture.ids(&fixture.alpha, "shared");
            fs::write(tool.join("calls"), "").unwrap();
            let password = if field == "delivered-value" {
                PEER_PASSWORD
            } else {
                ""
            };
            let output = fixture.cli_env(
                &publisher,
                &["registry", "publish"],
                &[
                    ("TEST_HUB_BETA_PASSWORD", password),
                    ("TEST_HUB_HISTORY_PRIVATE_PASSWORD", password),
                ],
            );
            registry_privacy_redacted(&output);
            let after = fixture.ids(&fixture.alpha, "shared");
            let calls = fs::read_to_string(tool.join("calls")).unwrap();
            let wrote = calls
                .lines()
                .any(|call| matches!(call, "init" | "backup" | "forget" | "prune"));
            eprintln!(
                "history {phase}/{field}: exit={:?}; snapshots={} -> {}; writes={wrote}",
                output.status.code(),
                before.len(),
                after.len()
            );
            if output.status.success() || after != before || wrote {
                violations.push(format!("{phase}/{field}"));
            }
            assert_eq!(
                fs::read(publisher.path("shared").join("registry.yaml")).unwrap(),
                authored
            );
            assert_eq!(fixture.ids(&beta, "bot"), old_peer);
            payload_at(&gamma.path("owned"));
            if field != "url" {
                assert!(!Path::new(gamma.repository("bot")).exists());
            }
        }
    }
    payload_at(&fixture.alpha.path("owned"));
    assert!(
        violations.is_empty(),
        "historical identifiers admitted: {violations:?}"
    );
}

#[test]
fn history_known_master_clone_is_refused_on_owner_publication_and_private_setup_and_sync() {
    let (mut fixture, beta, mut gamma) = registry_privacy_transition_fixture(false);
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &gamma]);
    let absent = [("TEST_HUB_GAMMA_PASSWORD", "")];
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &absent));
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let tool = Fixture::wrapper(&mut gamma);
    let private_env = [
        ("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD),
        ("TEST_HUB_BETA_PASSWORD", ""),
    ];
    success(&fixture.cli_env(&gamma, &["setup"], &private_env));
    registry_privacy_author_current(&fixture, &private_env);
    assert!(fixture.status(&gamma)["ready"].as_bool().unwrap());
    let established_paths = gamma.config["paths"].clone();
    fs::rename(
        gamma.repository("bot"),
        fixture.temp.path().join("fresh-private-original"),
    )
    .unwrap();
    success(
        &std::process::Command::new("cp")
            .args(["-a", "--", beta.repository("bot"), gamma.repository("bot")])
            .output()
            .unwrap(),
    );
    let password_file = fixture.temp.path().join("history-rotated-password");
    write(&password_file, REGISTRY_PRIVATE_PASSWORD);
    fs::set_permissions(&password_file, fs::Permissions::from_mode(0o600)).unwrap();
    success(&registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        PEER_PASSWORD,
        &[
            "key",
            "passwd",
            "--new-password-file",
            password_file.to_str().unwrap(),
        ],
    ));
    let original = fixture.restic(&beta, "bot", &["cat", "config"]);
    let clone = registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["cat", "config"],
    );
    success(&original);
    success(&clone);
    assert_eq!(
        serde_json::from_slice::<Value>(&original.stdout).unwrap()["id"],
        serde_json::from_slice::<Value>(&clone.stdout).unwrap()["id"]
    );
    failure(&registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        PEER_PASSWORD,
        &["snapshots", "--json"],
    ));
    let before = registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["snapshots", "--json"],
    );
    success(&before);
    let before: Value = serde_json::from_slice(&before.stdout).unwrap();
    let shared = fixture.ids(&fixture.alpha, "shared");
    let mut violations = Vec::new();
    registry_privacy_assert_current_base(&fixture);
    let owner_tool = Fixture::wrapper(&mut fixture.alpha);
    let output = fixture.cli_env(&fixture.alpha, &["registry", "publish"], &private_env);
    registry_privacy_redacted(&output);
    assert!(
        !text(&output).to_lowercase().contains("stale authoring"),
        "known-master publication must reach the privacy guard"
    );
    let after = fixture.ids(&fixture.alpha, "shared");
    let owner_calls = fs::read_to_string(owner_tool.join("calls")).unwrap();
    let wrote = owner_calls
        .lines()
        .any(|call| matches!(call, "init" | "backup" | "forget" | "prune"));
    eprintln!(
        "history known-master owner publication: exit={:?}; writes={wrote}; snapshots={} -> {}",
        output.status.code(),
        shared.len(),
        after.len()
    );
    if output.status.success() || wrote || after != shared {
        violations.push("owner publication");
    }
    let shared = after;
    for command in ["setup", "sync"] {
        gamma.config["paths"] = established_paths.clone();
        if command == "setup" {
            gamma.config["paths"]["state"] = json!(fixture.temp.path().join("clone-first-state"));
            gamma.config["paths"]["imports"] =
                json!(fixture.temp.path().join("clone-first-imports"));
        }
        gamma.save();
        fs::write(tool.join("calls"), "").unwrap();
        let output = fixture.cli_env(&gamma, &[command], &private_env);
        registry_privacy_redacted(&output);
        let after = registry_privacy_restic(
            &fixture,
            gamma.repository("bot"),
            REGISTRY_PRIVATE_PASSWORD,
            &["snapshots", "--json"],
        );
        success(&after);
        let after: Value = serde_json::from_slice(&after.stdout).unwrap();
        let calls = fs::read_to_string(tool.join("calls")).unwrap();
        let wrote = calls
            .lines()
            .any(|call| matches!(call, "init" | "backup" | "forget" | "prune"));
        eprintln!(
            "history private clone {command}: exit={:?}; writes={wrote}; snapshots={} -> {}",
            output.status.code(),
            before.as_array().unwrap().len(),
            after.as_array().unwrap().len()
        );
        if output.status.success() || wrote || after != before {
            violations.push(command);
        }
        assert_eq!(fixture.ids(&fixture.alpha, "shared"), shared);
        payload_at(&gamma.path("owned"));
    }
    assert!(
        violations.is_empty(),
        "known shared master accepted by private host: {violations:?}"
    );
}

#[test]
fn history_control_fresh_private_admission_works_with_retired_peer_and_private_secrets_absent_at_owner()
 {
    let (fixture, beta) = history_retired_fixture();
    let gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_GAMMA_PASSWORD");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &gamma]);
    let owner_env = [
        ("TEST_HUB_BETA_PASSWORD", ""),
        ("TEST_HUB_GAMMA_PASSWORD", ""),
    ];
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &owner_env));
    assert!(!Path::new(gamma.repository("bot")).exists());
    let private_env = [
        ("TEST_HUB_BETA_PASSWORD", ""),
        ("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD),
    ];
    for command in ["setup", "sync"] {
        success(&fixture.cli_env(&gamma, &[command], &private_env));
    }
    let snapshots = registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["snapshots", "--json"],
    );
    success(&snapshots);
    let snapshots: Vec<Value> = serde_json::from_slice(&snapshots.stdout).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert!(
        snapshots[0]["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("bot:gamma"))
    );
    for password in [SHARED_PASSWORD, PEER_PASSWORD] {
        failure(&registry_privacy_restic(
            &fixture,
            gamma.repository("bot"),
            password,
            &["snapshots", "--json"],
        ));
    }
    registry_privacy_author_current(&fixture, &owner_env);
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &owner_env));
    assert!(!fixture.ids(&beta, "bot").is_empty());
    payload_at(&gamma.path("owned"));
}

#[test]
fn history_generated_metadata_survives_owner_publication_and_contains_only_encrypted_shared_identifiers()
 {
    let (fixture, mut beta) = history_retired_fixture();
    let retired = beta.entry();
    let original = fixture.restic(&beta, "bot", &["cat", "config"]);
    success(&original);
    let original: Value = serde_json::from_slice(&original.stdout).unwrap();
    let peer_snapshots = fixture.ids(&beta, "bot");
    let old_shared = fixture.current_shared_id(&fixture.alpha);
    let gamma = registry_privacy_peer(&fixture, "gamma", false, "TEST_HUB_GAMMA_PASSWORD");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &gamma]);
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let private_env = [("TEST_HUB_GAMMA_PASSWORD", REGISTRY_PRIVATE_PASSWORD)];
    success(&fixture.cli_env(&gamma, &["setup"], &private_env));
    registry_privacy_author_current(&fixture, &private_env);
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &private_env));
    let private_id = registry_privacy_restic(
        &fixture,
        gamma.repository("bot"),
        REGISTRY_PRIVATE_PASSWORD,
        &["cat", "config"],
    );
    success(&private_id);
    let private_id: Value = serde_json::from_slice(&private_id.stdout).unwrap();
    let archived = fixture
        .temp
        .path()
        .join("retired-shared-repository-offline");
    fs::rename(beta.repository("bot"), &archived).unwrap();
    beta.config["repositories"]["bot"]["url"] = json!(archived);
    let successor = fixture.bot("alpha", "alpha-history-metadata-successor");
    fixture.ok(&fixture.alpha, &["setup"]);
    copy_tree(
        &fixture.active_shared(&fixture.alpha),
        &successor.path("shared"),
    );
    fs::remove_file(successor.path("shared").join(HISTORY_FILE)).unwrap();
    fixture.registry(&successor, &[&successor, &gamma]);
    assert!(!successor.path("state").exists());
    assert!(!successor.path("imports").exists());
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    fixture.ok(&successor, &["setup"]);
    success(&fixture.cli_env(&successor, &["registry", "publish"], &private_env));
    assert_eq!(fixture.ids(&beta, "bot"), peer_snapshots);
    for author in [&fixture.alpha, &successor] {
        assert!(
            !author.path("shared").join(HISTORY_FILE).exists(),
            "generated metadata must not be an authoring input"
        );
        payload_at(&author.path("owned"));
    }
    let restored = history_restored_shared(&fixture, "history-metadata-recovery");
    let bytes = history_generated_bytes(&restored);
    let shared_config = fixture.restic(&fixture.alpha, "shared", &["cat", "config"]);
    let owner_config = fixture.restic(&fixture.alpha, "bot", &["cat", "config"]);
    success(&shared_config);
    success(&owner_config);
    let shared_config: Value = serde_json::from_slice(&shared_config.stdout).unwrap();
    let owner_config: Value = serde_json::from_slice(&owner_config.stdout).unwrap();
    let identifiers = [
        retired["repository"].as_str().unwrap(),
        retired["password_ref"].as_str().unwrap(),
        retired["password_env"].as_str().unwrap(),
        original["id"].as_str().unwrap(),
        fixture.alpha.repository("shared"),
        "op://synthetic/shared/password",
        "TEST_HUB_SHARED_PASSWORD",
        shared_config["id"].as_str().unwrap(),
        fixture.alpha.repository("bot"),
        owner_config["id"].as_str().unwrap(),
    ];
    for identifier in identifiers {
        assert!(
            history_contains(&bytes, identifier),
            "generated metadata lost a known shared identifier"
        );
    }
    for identifier in [
        gamma.repository("bot"),
        "op://synthetic/gamma/password",
        "TEST_HUB_GAMMA_PASSWORD",
        private_id["id"].as_str().unwrap(),
        SHARED_PASSWORD,
        BOT_PASSWORD,
        PEER_PASSWORD,
        REGISTRY_PRIVATE_PASSWORD,
    ] {
        assert!(
            !history_contains(&bytes, identifier),
            "history retained private identifiers or resolved secret values"
        );
    }
    history_assert_encrypted(Path::new(fixture.alpha.repository("shared")), &identifiers);
    assert_no_plaintext(Path::new(fixture.alpha.repository("shared")));
    let old_copy = fixture.temp.path().join("old-peer-shared-recovery");
    success(&restore(&fixture, "shared", &old_shared, &old_copy));
    history_generated_bytes(&old_copy);
    assert!(old_copy.join("registry.yaml").is_file());
}

// Deliberately install a raw accepted state to exercise import validation. A
// newer restic timestamp alone cannot make this tree authoritative in v2.
fn history_accept_shared_snapshot(fixture: &Fixture, source: &Path) {
    let mut head = fixture.shared_head(&fixture.alpha);
    let revision = head["revision"].as_u64().unwrap() + 1;
    write_shared_revision(&fixture.alpha, source, revision);
    let mut view = Bot {
        file: fixture.alpha.file.clone(),
        config: fixture.alpha.config.clone(),
    };
    view.config["paths"]["shared"] = json!(source);
    let hub = format!(
        "hub:{}",
        fixture.alpha.config["hub"].as_str().unwrap_or("default")
    );
    let output = fixture.restic(
        &view,
        "shared",
        &[
            "backup",
            ".",
            "--json",
            "--host",
            "alpha",
            "--tag",
            "utsurigi-agent-hub",
            "--tag",
            &hub,
            "--tag",
            "kind:shared-v2",
            "--tag",
            "bot:alpha",
            "--tag",
            "runtime:alpha-1",
        ],
    );
    success(&output);
    let snapshot = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|line| line["snapshot_id"].as_str().map(str::to_owned))
        .expect("raw immutable backup must return its snapshot ID");
    head["revision"] = json!(revision);
    head["snapshot"] = json!(snapshot);
    fs::write(
        Path::new(fixture.alpha.repository("shared")).join("uah-head.json"),
        serde_json::to_vec(&head).unwrap(),
    )
    .unwrap();
}

#[test]
fn history_missing_or_malformed_remote_metadata_blocks_imports_writes_and_prune() {
    let mut fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    for bot in [&fixture.alpha, &beta] {
        fixture.ok(bot, &["setup"]);
        fixture.ok(bot, &["sync"]);
    }
    let source = history_restored_shared(&fixture, "valid-history-source");
    let valid = history_generated_bytes(&source);
    let mut wrong_version: Value = serde_json::from_slice(&valid).unwrap();
    let version = wrong_version
        .get_mut("version")
        .expect("generated history must declare its format version");
    *version = json!(
        version
            .as_u64()
            .expect("history version must be an integer")
            + 1
    );
    let wrong_version = serde_json::to_vec(&wrong_version).unwrap();
    let authored = fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap();
    let previous = fixture.status(&fixture.alpha);
    let peer = fixture.ids(&beta, "bot");
    let owned = fixture.ids(&fixture.alpha, "bot");
    for (case, bytes) in [
        ("missing", None),
        ("truncated-json", Some(b"{".as_slice())),
        ("unsupported-version", Some(wrong_version.as_slice())),
        ("null", Some(b"null".as_slice())),
        ("array", Some(b"[]".as_slice())),
        ("empty-object", Some(b"{}".as_slice())),
    ] {
        fs::write(source.join(HISTORY_FILE), &valid).unwrap();
        if let Some(bytes) = bytes {
            fs::write(source.join(HISTORY_FILE), bytes).unwrap();
        } else {
            fs::remove_file(source.join(HISTORY_FILE)).unwrap();
        }
        history_accept_shared_snapshot(&fixture, &source);
        let before = fixture.ids(&fixture.alpha, "shared");
        let inspection = history_restored_shared(&fixture, &format!("inspection-{case}"));
        if let Some(bytes) = bytes {
            assert_eq!(fs::read(inspection.join(HISTORY_FILE)).unwrap(), bytes);
        } else {
            assert!(!inspection.join(HISTORY_FILE).exists());
        }
        let destination = beta.path("imports").join(format!("denied-{case}"));
        for args in [
            vec!["setup"],
            vec!["sync"],
            vec!["registry", "publish"],
            vec!["prune"],
            vec![
                "pull",
                "beta",
                "--destination",
                destination.to_str().unwrap(),
            ],
        ] {
            fs::write(tool.join("calls"), "").unwrap();
            let output = fixture.cli(&fixture.alpha, &args);
            eprintln!(
                "history invalid {case}/{args:?}: exit={:?}",
                output.status.code()
            );
            failure(&output);
            assert_no_writes(&tool);
            assert_eq!(fixture.ids(&fixture.alpha, "shared"), before);
            assert_eq!(fixture.ids(&fixture.alpha, "bot"), owned);
            assert_eq!(fixture.ids(&beta, "bot"), peer);
            assert_eq!(fixture.status(&fixture.alpha), previous);
            assert_eq!(
                fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap(),
                authored
            );
            assert!(!destination.exists());
            payload_at(&fixture.alpha.path("owned"));
            payload_at(&beta.path("owned"));
        }
    }
}

#[test]
fn history_reserved_authoring_collision_refuses_fresh_setup_and_publication_without_overwrite() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let collision = fixture.alpha.path("shared").join(HISTORY_FILE);
    let sentinel = b"synthetic author-owned history file; preserve these exact bytes";
    fs::write(&collision, sentinel).unwrap();
    let mut violations = Vec::new();
    let first = fixture.cli(&fixture.alpha, &["setup"]);
    redacted(&first);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    let wrote = calls
        .lines()
        .any(|call| matches!(call, "init" | "backup" | "forget" | "prune"));
    eprintln!(
        "history initial authoring collision: exit={:?}; writes={wrote}",
        first.status.code()
    );
    if first.status.success()
        || wrote
        || Path::new(fixture.alpha.repository("shared")).exists()
        || Path::new(fixture.alpha.repository("bot")).exists()
    {
        violations.push("fresh setup");
    }
    assert_eq!(fs::read(&collision).unwrap(), sentinel);
    payload_at(&fixture.alpha.path("owned"));

    fs::remove_file(&collision).unwrap();
    fixture.ok(&fixture.alpha, &["setup"]);
    let before = fixture.ids(&fixture.alpha, "shared");
    let owned = fixture.ids(&fixture.alpha, "bot");
    fs::write(&collision, sentinel).unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    let output = fixture.cli(&fixture.alpha, &["registry", "publish"]);
    redacted(&output);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    let wrote = calls
        .lines()
        .any(|call| matches!(call, "init" | "backup" | "forget" | "prune"));
    let after = fixture.ids(&fixture.alpha, "shared");
    eprintln!(
        "history established publication collision: exit={:?}; writes={wrote}; snapshots={} -> {}",
        output.status.code(),
        before.len(),
        after.len()
    );
    if output.status.success() || wrote || before != after {
        violations.push("established publication");
    }
    assert_eq!(fs::read(&collision).unwrap(), sentinel);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), owned);
    payload_at(&fixture.alpha.path("owned"));
    assert!(
        violations.is_empty(),
        "reserved authoring history was published: {violations:?}"
    );
}

const HISTORY_R2_ENV: [(&str, &str); 2] = [
    ("AWS_ACCESS_KEY_ID", "synthetic-access-key"),
    ("AWS_SECRET_ACCESS_KEY", BOT_PASSWORD),
];

fn history_r2_fixture() -> (Fixture, Bot, PathBuf, PathBuf) {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let local = Bot {
        file: fixture.alpha.file.clone(),
        config: fixture.alpha.config.clone(),
    };
    fs::write(tool.join("repo-shared"), local.repository("shared")).unwrap();
    fs::write(tool.join("repo-bot"), local.repository("bot")).unwrap();
    fixture.alpha.config["storage"]["provider"] = json!("r2");
    fixture.alpha.config["repositories"]["shared"]["url"] = json!(R2_SHARED);
    fixture.alpha.config["repositories"]["bot"]["url"] = json!(R2_BOT);
    let attestation = fixture.temp.path().join("history-attestation.json");
    fixture.alpha.config["budget"]["attestation_file"] = json!(attestation);
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    fixture.fake_r2_head(&fixture.alpha, local.repository("shared"));
    history_attest(&fixture, &attestation, 1_000_000);
    (fixture, local, tool, attestation)
}

fn history_attest(fixture: &Fixture, path: &Path, max_source_bytes: u64) {
    fs::write(
        path,
        json!({
            "expires_at": "2999-01-01T00:00:00Z",
            "repositories": [fixture.alpha.repository("shared"), fixture.alpha.repository("bot")],
            "max_source_bytes": max_source_bytes,
        })
        .to_string(),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn history_tree_bytes(root: &Path) -> u64 {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                history_tree_bytes(&path)
            } else {
                path.metadata().unwrap().len()
            }
        })
        .sum()
}

#[test]
fn history_initial_budget_includes_generated_metadata_before_any_backup() {
    let (fixture, local, tool, attestation) = history_r2_fixture();
    let authoring_bytes = history_tree_bytes(&fixture.alpha.path("shared"));
    assert!(authoring_bytes > history_tree_bytes(&fixture.alpha.path("owned")));
    history_attest(&fixture, &attestation, authoring_bytes);
    let output = fixture.cli_env(&fixture.alpha, &["setup"], &HISTORY_R2_ENV);
    redacted(&output);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    let backed_up = calls
        .lines()
        .any(|call| matches!(call, "backup" | "forget" | "prune"));
    eprintln!(
        "history initial budget: exit={:?}; backup_or_prune={backed_up}; authoring_bytes={authoring_bytes}",
        output.status.code()
    );
    assert!(
        !output.status.success() && !backed_up,
        "authoring-only authorization allowed generated history backup"
    );
    assert!(!fixture.alpha.path("shared").join(HISTORY_FILE).exists());
    payload_at(&fixture.alpha.path("owned"));
    history_attest(&fixture, &attestation, 1_000_000);
    success(&fixture.cli_env(&fixture.alpha, &["setup"], &HISTORY_R2_ENV));
    let snapshots = fixture.content_snapshots(&local, "shared");
    assert_eq!(snapshots.len(), 1);
    let destination = fixture.temp.path().join("initial-budget-generated-history");
    success(&fixture.cli_env(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "shared",
            "--snapshot",
            snapshots[0]["id"].as_str().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
        ],
        &HISTORY_R2_ENV,
    ));
    history_generated_bytes(&destination);
}

#[test]
fn history_publication_budget_counts_actual_generated_bytes_at_the_exact_boundary() {
    let (fixture, local, tool, attestation) = history_r2_fixture();
    success(&fixture.cli_env(&fixture.alpha, &["setup"], &HISTORY_R2_ENV));
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &HISTORY_R2_ENV));
    let current = fixture.current_shared_id(&local);
    let destination = fixture
        .temp
        .path()
        .join("publication-budget-generated-history");
    success(&fixture.cli_env(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "shared",
            "--snapshot",
            &current,
            "--destination",
            destination.to_str().unwrap(),
        ],
        &HISTORY_R2_ENV,
    ));
    let generated = history_generated_bytes(&destination);
    let total = history_tree_bytes(&destination);
    let authoring_bytes = history_tree_bytes(&fixture.alpha.path("shared"));
    assert_eq!(total, authoring_bytes + generated.len() as u64);
    let before = fixture.ids(&local, "shared");
    let previous = fixture.status(&fixture.alpha);
    let authored = fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap();
    history_attest(&fixture, &attestation, total - 1);
    fs::write(tool.join("calls"), "").unwrap();
    let output = fixture.cli_env(&fixture.alpha, &["registry", "publish"], &HISTORY_R2_ENV);
    failure(&output);
    assert_no_writes(&tool);
    assert_eq!(fixture.ids(&local, "shared"), before);
    assert_eq!(fixture.status(&fixture.alpha), previous);
    assert_eq!(
        fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap(),
        authored
    );
    history_attest(&fixture, &attestation, total);
    fs::write(tool.join("calls"), "").unwrap();
    success(&fixture.cli_env(&fixture.alpha, &["registry", "publish"], &HISTORY_R2_ENV));
    let after = fixture.ids(&local, "shared");
    assert!(before.is_subset(&after));
    assert_eq!(after.len(), before.len() + 1);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|call| *call == "backup").count(), 1);
    assert!(!fixture.alpha.path("shared").join(HISTORY_FILE).exists());
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn hub_default_status_and_backups_have_one_membership_marker() {
    let mut fixture = Fixture::new();
    fixture.alpha.config.as_object_mut().unwrap().remove("hub");
    fixture.alpha.save();
    fixture.ok(&fixture.alpha, &["validate"]);
    let output = fixture.ok(&fixture.alpha, &["status"]);
    assert!(!fixture.alpha.path("state").exists());
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let snapshots = ["shared", "bot"].map(|repository| {
        let snapshots = fixture.content_snapshots(&fixture.alpha, repository);
        assert_eq!(snapshots.len(), 1);
        snapshots
    });
    eprintln!(
        "default membership positive control: validate, read-only status, real-restic setup and sync succeeded"
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        status["hub"], "default",
        "status must report the default hub"
    );
    assert_eq!(fixture.status(&fixture.alpha)["hub"], "default");
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.alpha.path("state").join("status.json")).unwrap())
            .unwrap();
    assert_eq!(state["hub"], "default");
    for snapshot in snapshots.into_iter().flatten() {
        let hubs: Vec<_> = snapshot["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tag| tag.as_str().unwrap().strip_prefix("hub:"))
            .collect();
        assert_eq!(
            hubs,
            ["default"],
            "each backup needs exactly one hub marker"
        );
    }
}

fn hub_rejected(output: &std::process::Output) {
    failure(output);
    assert_eq!(output.status.code(), Some(1), "{}", text(output));
    assert!(
        text(output).to_ascii_lowercase().contains("hub"),
        "{}",
        text(output)
    );
    assert!(
        !text(output).contains("UNTRUSTED-HUB-79c1"),
        "untrusted hub tag was printed"
    );
}

fn hub_schema_rejected(output: &std::process::Output, token: &str) {
    failure(output);
    assert_eq!(output.status.code(), Some(1), "{}", text(output));
    assert!(text(output).contains(token), "{}", text(output));
}

fn hub_tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(hub_tree(&path));
        } else {
            files.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    files
}

#[test]
fn hub_config_accepts_identity_names_and_rejects_invalid_names_and_types() {
    for name in [
        "default".to_owned(),
        "work_2.A-1".to_owned(),
        "123".to_owned(),
        "h".repeat(128),
    ] {
        for document in [
            format!("hub: {}\n", json!(name)),
            json!({"hub": name}).to_string(),
        ] {
            success(&strict_template_validation(&document));
        }
    }
    for name in [
        "".to_owned(),
        ".".to_owned(),
        "..".to_owned(),
        "a/b".to_owned(),
        "a\\b".to_owned(),
        "two hubs".to_owned(),
        "日本語".to_owned(),
        "h".repeat(129),
    ] {
        for document in [
            format!("hub: {}\n", json!(name)),
            json!({"hub": name}).to_string(),
        ] {
            hub_rejected(&strict_template_validation(&document));
        }
    }
    for scalar in ["123", "1.5", "true", "null", "[]", "{}"] {
        for document in [format!("hub: {scalar}\n"), format!("{{\"hub\":{scalar}}}")] {
            hub_schema_rejected(
                &strict_template_validation(&document),
                "Invalid configuration",
            );
        }
    }
}

#[test]
fn hub_status_reports_default_and_named_membership_without_runtime_paths() {
    let temp = TempDir::new();
    let file = temp.path().join("config.yaml");
    for (config, expected) in [
        (json!({}), "default"),
        (json!({"hub": "default"}), "default"),
        (json!({"hub": "personal"}), "personal"),
    ] {
        fs::write(&file, config.to_string()).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .current_dir(temp.path())
            .arg("--config")
            .arg(&file)
            .arg("status")
            .output()
            .unwrap();
        success(&output);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["hub"], expected);
        assert_eq!(status["ready"], false);
        for field in ["last_success", "anchor", "next_due", "active_shared"] {
            assert_eq!(status[field], Value::Null);
        }
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            1,
            "status must not create state or lock files"
        );
    }
}

#[test]
fn hub_named_membership_shares_across_platform_metadata_and_recovers() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["hub"] = json!("personal");
    fixture.alpha.config["bot"]["platform"] = json!("claude-code");
    fixture.alpha.save();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["hub"] = json!("personal");
    beta.config["bot"]["platform"] = json!("codex");
    beta.save();
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"personal hub knowledge",
    );
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    for bot in [&fixture.alpha, &beta] {
        assert_eq!(fixture.status(bot)["hub"], "personal");
        assert!(!bot.path("state").exists());
        fixture.ok(bot, &["setup"]);
        fixture.ok(bot, &["sync"]);
        assert_eq!(fixture.status(bot)["hub"], "personal");
        let state: Value =
            serde_json::from_slice(&fs::read(bot.path("state").join("status.json")).unwrap())
                .unwrap();
        assert_eq!(state["hub"], "personal");
        for repository in ["shared", "bot"] {
            for snapshot in fixture.content_snapshots(bot, repository) {
                let tags = snapshot["tags"].as_array().unwrap();
                let hubs: Vec<_> = tags
                    .iter()
                    .filter_map(|tag| tag.as_str().unwrap().strip_prefix("hub:"))
                    .collect();
                assert_eq!(hubs, ["personal"]);
            }
        }
    }
    let destination = beta.path("imports").join("alpha-copy");
    fixture.ok(
        &beta,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    payload_at(&destination);
    for repository in ["shared", "bot"] {
        let snapshot = if repository == "shared" {
            fixture.current_shared_id(&fixture.alpha)
        } else {
            fixture
                .ids(&fixture.alpha, repository)
                .first()
                .unwrap()
                .clone()
        };
        let destination = fixture
            .temp
            .path()
            .join(format!("named-{repository}-recovery"));
        success(&restore(&fixture, repository, &snapshot, &destination));
        if repository == "bot" {
            payload_at(&destination);
        } else {
            assert_eq!(
                fs::read(destination.join("knowledge.md")).unwrap(),
                b"personal hub knowledge"
            );
            let registry: Value =
                serde_json::from_slice(&fs::read(destination.join("registry.yaml")).unwrap())
                    .unwrap();
            assert_eq!(registry["hub"], "personal");
        }
    }
}

#[test]
fn hub_same_bot_id_has_separate_memberships_roots_repositories_and_schedules() {
    let fixture = Fixture::new();
    write(
        fixture.alpha.path("shared").join("only-first.md"),
        b"first hub only",
    );
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let original_state = fs::read(fixture.alpha.path("state").join("status.json")).unwrap();
    let original_import = fixture.active_shared(&fixture.alpha);
    let original_tree = hub_tree(&original_import);
    let original_repositories =
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository));
    let mut other = Fixture::make_bot(&fixture.temp.path().join("second-hub"), "alpha", "alpha-1");
    other.config["hub"] = json!("default-2");
    other.config["sync"] = json!({"start_time": "05:00"});
    other.save();
    write(
        other.path("owned").join("notes/日本語.md"),
        b"second hub only",
    );
    fixture.registry(&other, &[&other]);
    for key in ["owned", "shared", "imports", "state"] {
        assert_ne!(other.path(key), fixture.alpha.path(key));
    }
    for repository in ["shared", "bot"] {
        assert_ne!(
            other.repository(repository),
            fixture.alpha.repository(repository)
        );
    }
    fixture.ok(&other, &["setup"]);
    fixture.ok(&other, &["sync"]);
    assert_eq!(fixture.status(&other)["hub"], "default-2");
    assert_ne!(
        fixture.status(&other)["anchor"],
        fixture.status(&fixture.alpha)["anchor"]
    );
    assert!(!other.path("shared").join("only-first.md").exists());
    assert!(!fixture.active_shared(&other).join("only-first.md").exists());
    assert_eq!(
        fs::read(fixture.alpha.path("state").join("status.json")).unwrap(),
        original_state
    );
    assert_eq!(hub_tree(&original_import), original_tree);
    assert_eq!(
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository)),
        original_repositories
    );
    let snapshots = fixture.ids(&other, "bot");
    let destination = fixture.temp.path().join("second-hub-recovery");
    fixture.ok(
        &other,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            snapshots.first().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        b"second hub only"
    );
    payload_at(&fixture.alpha.path("owned"));
}

#[test]
fn hub_local_registry_rejects_foreign_missing_and_nonstring_membership_before_publication() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let file = fixture.alpha.path("shared").join("registry.yaml");
    let valid: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let active = fixture.active_shared(&fixture.alpha);
    let imported = hub_tree(&active);
    let owned = hub_tree(&fixture.alpha.path("owned"));
    let repositories =
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository));
    for (case, value) in [
        ("foreign", Some(json!("default-2"))),
        ("missing", None),
        ("number", Some(json!(123))),
        ("null", Some(Value::Null)),
    ] {
        let mut invalid = valid.clone();
        if let Some(value) = value {
            invalid["hub"] = value;
        } else {
            invalid.as_object_mut().unwrap().remove("hub");
        }
        let documents = if case == "missing" {
            [
                (
                    "YAML",
                    format!(
                        "version: {}\nmax_bots: {}\nbots: {}\n",
                        invalid["version"], invalid["max_bots"], invalid["bots"]
                    ),
                ),
                ("JSON", invalid.to_string()),
            ]
        } else {
            strict_registry_documents(&invalid)
        };
        for (format, document) in documents {
            fs::write(&file, &document).unwrap();
            fs::write(tool.join("calls"), "").unwrap();
            let state = fs::read(fixture.alpha.path("state").join("status.json")).unwrap();
            let output = fixture.cli(&fixture.alpha, &["registry", "publish"]);
            eprintln!("local registry {case}/{format}: {}", text(&output));
            if case == "foreign" {
                hub_rejected(&output);
            } else {
                hub_schema_rejected(&output, "Invalid registry schema");
            }
            assert_no_writes(&tool);
            assert_eq!(fs::read_to_string(&file).unwrap(), document);
            assert_state_data_unchanged(&fixture.alpha, &state, "publish");
            assert_eq!(hub_tree(&active), imported);
            assert_eq!(hub_tree(&fixture.alpha.path("owned")), owned);
            assert_eq!(
                ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository)),
                repositories
            );
        }
    }
    let mut duplicate = valid.clone();
    let mut entry = duplicate["bots"][0].clone();
    entry["repository"] = json!(fixture.temp.path().join("duplicate-repository"));
    duplicate["bots"].as_array_mut().unwrap().push(entry);
    fs::write(&file, duplicate.to_string()).unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    hub_schema_rejected(
        &fixture.cli(&fixture.alpha, &["registry", "publish"]),
        "Invalid or duplicate registry identity/repository",
    );
    assert_no_writes(&tool);
    fs::write(&file, valid.to_string()).unwrap();
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
}

#[test]
fn hub_imported_registry_rejects_foreign_missing_and_nonstring_membership_without_adoption() {
    let mut fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    for bot in [&fixture.alpha, &beta] {
        fixture.ok(bot, &["setup"]);
        fixture.ok(bot, &["sync"]);
    }
    let source = history_restored_shared(&fixture, "hub-registry-source");
    let valid = fs::read(source.join("registry.yaml")).unwrap();
    for (case, value) in [
        ("foreign", Some(json!("default-2"))),
        ("missing", None),
        ("number", Some(json!(123))),
    ] {
        fs::write(source.join("registry.yaml"), &valid).unwrap();
        history_accept_shared_snapshot(&fixture, &source);
        fixture.ok(&fixture.alpha, &["setup"]);
        fixture.ok(&fixture.alpha, &["sync"]);
        fixture.author_current(&fixture.alpha);
        fixture.ok(&fixture.alpha, &["registry", "publish"]);
        fixture.ok(&fixture.alpha, &["prune"]);
        let authored = fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap();
        let control = fixture
            .temp
            .path()
            .join(format!("hub-import-control-{case}"));
        fixture.ok(
            &fixture.alpha,
            &["pull", "beta", "--destination", control.to_str().unwrap()],
        );
        payload_at(&control);
        let mut invalid: Value = serde_json::from_slice(&valid).unwrap();
        if let Some(value) = value {
            invalid["hub"] = value;
        } else {
            invalid.as_object_mut().unwrap().remove("hub");
        }
        fs::write(source.join("registry.yaml"), invalid.to_string()).unwrap();
        history_accept_shared_snapshot(&fixture, &source);
        let active = fixture.active_shared(&fixture.alpha);
        let imported = hub_tree(&active);
        let owned = hub_tree(&fixture.alpha.path("owned"));
        let repositories =
            ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository));
        let peers = fixture.snapshots(&beta, "bot");
        let destination = fixture
            .temp
            .path()
            .join(format!("hub-import-denied-{case}"));
        for args in [
            vec!["setup"],
            vec!["sync"],
            vec!["registry", "publish"],
            vec!["prune"],
            vec![
                "pull",
                "beta",
                "--destination",
                destination.to_str().unwrap(),
            ],
        ] {
            fs::write(tool.join("calls"), "").unwrap();
            let state = fs::read(fixture.alpha.path("state").join("status.json")).unwrap();
            let output = fixture.cli(&fixture.alpha, &args);
            eprintln!("imported registry {case}/{args:?}: {}", text(&output));
            if case == "foreign" {
                hub_rejected(&output);
            } else {
                hub_schema_rejected(&output, "Invalid registry schema");
            }
            assert_no_writes(&tool);
            assert!(!destination.exists());
            assert_state_data_unchanged(
                &fixture.alpha,
                &state,
                if args[0] == "registry" {
                    "publish"
                } else {
                    args[0]
                },
            );
            assert_eq!(hub_tree(&active), imported);
            assert_eq!(hub_tree(&fixture.alpha.path("owned")), owned);
            assert_eq!(
                fs::read(fixture.alpha.path("shared").join("registry.yaml")).unwrap(),
                authored
            );
            assert_eq!(
                ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository)),
                repositories
            );
            assert_eq!(fixture.snapshots(&beta, "bot"), peers);
            payload_at(&beta.path("owned"));
        }
    }
}

#[test]
fn hub_snapshot_guards_inspect_all_records_and_preserve_data_on_rejection() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    for repository in ["shared", "bot"] {
        for case in [
            "missing-complete",
            "duplicate-complete",
            "prefix-complete",
            "foreign-incomplete",
            "missing-incomplete",
            "duplicate-incomplete",
            "foreign-nonframework",
        ] {
            let missing = case.starts_with("missing-");
            let duplicate = case.starts_with("duplicate-");
            let before = fixture.ids(&fixture.alpha, repository);
            let source = if case.ends_with("incomplete") || case.ends_with("nonframework") {
                let mut args = vec!["backup", ".", "--host", "alpha", "--tag", "hub:default"];
                if case.ends_with("incomplete") {
                    args.extend([
                        "--tag",
                        "utsurigi-agent-hub",
                        "--tag",
                        if repository == "shared" {
                            "kind:shared-v2"
                        } else {
                            "kind:bot"
                        },
                        "--tag",
                        "bot:alpha",
                        "--tag",
                        "runtime:alpha-1",
                    ]);
                }
                success(&fixture.restic(&fixture.alpha, repository, &args));
                let after = fixture.ids(&fixture.alpha, repository);
                let added: Vec<_> = after.difference(&before).cloned().collect();
                assert_eq!(added.len(), 1);
                added[0].clone()
            } else if repository == "shared" {
                fixture.current_shared_id(&fixture.alpha)
            } else {
                fixture
                    .snapshots(&fixture.alpha, repository)
                    .into_iter()
                    .find(|snapshot| {
                        snapshot["tags"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
                    })
                    .unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            fixture.ok(&fixture.alpha, &["sync"]);
            fixture.ok(&fixture.alpha, &["prune"]);
            let good = if repository == "shared" {
                fixture.current_shared_id(&fixture.alpha)
            } else {
                fixture
                    .snapshots(&fixture.alpha, repository)
                    .into_iter()
                    .find(|snapshot| {
                        snapshot["tags"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
                    })
                    .unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            let control = fixture
                .temp
                .path()
                .join(format!("hub-marker-control-{repository}-{case}"));
            success(&restore(&fixture, repository, &good, &control));
            if repository == "bot" {
                payload_at(&control);
            } else {
                assert!(control.join("registry.yaml").is_file());
            }
            let before_tag = fixture.ids(&fixture.alpha, repository);
            let bad_marker = if duplicate {
                "hub:UNTRUSTED-HUB-79c1"
            } else {
                "hub:default-2"
            };
            let mut args = vec!["tag"];
            if !duplicate {
                args.extend(["--remove", "hub:default"]);
            }
            if !missing {
                args.extend(["--add", bad_marker]);
            }
            args.push(&source);
            success(&fixture.restic(&fixture.alpha, repository, &args));
            let after_tag = fixture.ids(&fixture.alpha, repository);
            let added: Vec<_> = after_tag.difference(&before_tag).cloned().collect();
            assert_eq!(added.len(), 1);
            let foreign = &added[0];
            if repository == "shared" {
                let mut head = fixture.shared_head(&fixture.alpha);
                if head["snapshot"] == source {
                    head["snapshot"] = json!(foreign);
                    fs::write(
                        Path::new(fixture.alpha.repository("shared")).join("uah-head.json"),
                        serde_json::to_vec(&head).unwrap(),
                    )
                    .unwrap();
                }
            }
            let active = fixture.active_shared(&fixture.alpha);
            let imported = hub_tree(&active);
            let owned = hub_tree(&fixture.alpha.path("owned"));
            let repositories =
                ["shared", "bot"].map(|name| fixture.snapshots(&fixture.alpha, name));
            let destination = fixture
                .temp
                .path()
                .join(format!("hub-marker-denied-{repository}-{case}"));
            for args in [
                vec!["setup"],
                vec!["sync"],
                vec!["prune"],
                vec![
                    "restore",
                    "--repository",
                    repository,
                    "--snapshot",
                    &good,
                    "--destination",
                    destination.to_str().unwrap(),
                ],
                vec![
                    "restore",
                    "--repository",
                    repository,
                    "--snapshot",
                    foreign,
                    "--destination",
                    destination.to_str().unwrap(),
                ],
            ] {
                fs::write(tool.join("calls"), "").unwrap();
                let state = fs::read(fixture.alpha.path("state").join("status.json")).unwrap();
                let output = fixture.cli(&fixture.alpha, &args);
                eprintln!(
                    "snapshot guard {repository}/{case}: exit={:?}",
                    output.status.code()
                );
                hub_rejected(&output);
                assert_no_writes(&tool);
                assert!(!destination.exists());
                assert_state_data_unchanged(&fixture.alpha, &state, args[0]);
                assert_eq!(hub_tree(&active), imported);
                assert_eq!(hub_tree(&fixture.alpha.path("owned")), owned);
                assert_eq!(
                    ["shared", "bot"].map(|name| fixture.snapshots(&fixture.alpha, name)),
                    repositories
                );
            }
            let mut args = vec!["tag"];
            if !missing {
                args.extend(["--remove", bad_marker]);
            }
            if !duplicate {
                args.extend(["--add", "hub:default"]);
            }
            args.push(foreign);
            let before_repair = fixture.ids(&fixture.alpha, repository);
            success(&fixture.restic(&fixture.alpha, repository, &args));
            if repository == "shared" {
                let mut head = fixture.shared_head(&fixture.alpha);
                if head["snapshot"] == *foreign {
                    let repaired = fixture.ids(&fixture.alpha, repository);
                    let added: Vec<_> = repaired.difference(&before_repair).collect();
                    assert_eq!(added.len(), 1);
                    head["snapshot"] = json!(added[0]);
                    fs::write(
                        Path::new(fixture.alpha.repository("shared")).join("uah-head.json"),
                        serde_json::to_vec(&head).unwrap(),
                    )
                    .unwrap();
                }
            }
            fixture.ok(&fixture.alpha, &["sync"]);
        }
    }
}

#[test]
fn hub_peer_snapshot_mismatch_cannot_publish_an_import() {
    let mut fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    for bot in [&fixture.alpha, &beta] {
        fixture.ok(bot, &["setup"]);
        fixture.ok(bot, &["sync"]);
    }
    let control = fixture.temp.path().join("hub-peer-control");
    fixture.ok(
        &fixture.alpha,
        &["pull", "beta", "--destination", control.to_str().unwrap()],
    );
    payload_at(&control);
    let peer = fixture.ids(&beta, "bot");
    success(&fixture.restic(
        &beta,
        "bot",
        &[
            "tag",
            "--remove",
            "hub:default",
            "--add",
            "hub:default-2",
            peer.first().unwrap(),
        ],
    ));
    let peers = fixture.snapshots(&beta, "bot");
    let repositories =
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository));
    let state = fs::read(fixture.alpha.path("state").join("status.json")).unwrap();
    let active = fixture.active_shared(&fixture.alpha);
    let imported = hub_tree(&active);
    let owned = hub_tree(&fixture.alpha.path("owned"));
    let destination = fixture.temp.path().join("hub-peer-denied");
    fs::write(tool.join("calls"), "").unwrap();
    hub_rejected(&fixture.cli(
        &fixture.alpha,
        &[
            "pull",
            "beta",
            "--destination",
            destination.to_str().unwrap(),
        ],
    ));
    assert_no_writes(&tool);
    assert!(!destination.exists());
    assert_state_data_unchanged(&fixture.alpha, &state, "pull");
    assert_eq!(hub_tree(&active), imported);
    assert_eq!(hub_tree(&fixture.alpha.path("owned")), owned);
    assert_eq!(
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository)),
        repositories
    );
    assert_eq!(fixture.snapshots(&beta, "bot"), peers);
    payload_at(&beta.path("owned"));
}

#[test]
fn hub_initial_setup_checks_existing_bot_repository_before_initializing_shared() {
    let fixture = Fixture::new();
    success(&fixture.restic(&fixture.alpha, "bot", &["init"]));
    success(&fixture.restic(
        &fixture.alpha,
        "bot",
        &["backup", ".", "--tag", "hub:default"],
    ));
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let mut candidate = Fixture::make_bot(
        &fixture.temp.path().join("wrong-hub-owner"),
        "alpha",
        "alpha-1",
    );
    fixture.registry(&candidate, &[&candidate]);
    success(&fixture.restic(&candidate, "bot", &["init"]));
    success(&fixture.restic(
        &candidate,
        "bot",
        &["backup", ".", "--tag", "hub:default-2"],
    ));
    let tool = Fixture::wrapper(&mut candidate);
    let before = fixture.snapshots(&candidate, "bot");
    let owned = hub_tree(&candidate.path("owned"));
    fs::write(tool.join("calls"), "").unwrap();
    hub_rejected(&fixture.cli(&candidate, &["setup"]));
    assert_no_writes(&tool);
    assert!(
        !Path::new(candidate.repository("shared"))
            .join("config")
            .exists()
    );
    let missing = fixture.restic(&candidate, "shared", &["snapshots", "--json"]);
    assert_eq!(
        missing.status.code(),
        Some(10),
        "shared repository was initialized: {}",
        text(&missing)
    );
    let status = fixture.status(&candidate);
    for field in ["last_success", "anchor", "next_due", "active_shared"] {
        assert_eq!(
            status[field],
            Value::Null,
            "failed initial setup must not record successful progress"
        );
    }
    assert_eq!(fixture.snapshots(&candidate, "bot"), before);
    assert_eq!(hub_tree(&candidate.path("owned")), owned);
}

#[test]
fn hub_persisted_state_rejects_foreign_missing_and_nonstring_membership_before_status_or_skip() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let file = fixture.alpha.path("state").join("status.json");
    let mut valid: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    valid["next_due"] = json!(chrono::Utc::now().timestamp() + 86400);
    fs::write(&file, valid.to_string()).unwrap();
    let active = fixture.active_shared(&fixture.alpha);
    let imported = hub_tree(&active);
    let owned = hub_tree(&fixture.alpha.path("owned"));
    let repositories =
        ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository));
    for (case, value) in [
        ("foreign", Some(json!("default-2"))),
        ("missing", None),
        ("number", Some(json!(123))),
        ("null", Some(Value::Null)),
    ] {
        fs::write(&file, valid.to_string()).unwrap();
        assert_eq!(
            fixture.status(&fixture.alpha)["active_shared"],
            json!(active)
        );
        let skipped = fixture.ok(&fixture.alpha, &["sync", "--scheduled"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&skipped.stdout).unwrap()["skipped"],
            true
        );
        let mut invalid = valid.clone();
        if let Some(value) = value {
            invalid["hub"] = value;
        } else {
            invalid.as_object_mut().unwrap().remove("hub");
        }
        fs::write(&file, invalid.to_string()).unwrap();
        let bytes = fs::read(&file).unwrap();
        for args in [vec!["status"], vec!["sync", "--scheduled"]] {
            fs::write(tool.join("calls"), "").unwrap();
            let output = fixture.cli(&fixture.alpha, &args);
            eprintln!("persisted state {case}/{args:?}: {}", text(&output));
            if case == "foreign" {
                hub_rejected(&output);
            } else {
                hub_schema_rejected(&output, "Invalid persisted state");
            }
            assert!(
                output.stdout.is_empty(),
                "rejected state returned active_shared or skipped output"
            );
            assert!(
                fs::read_to_string(tool.join("calls")).unwrap().is_empty(),
                "state must be checked before opening restic"
            );
            assert_eq!(fs::read(&file).unwrap(), bytes);
            assert_eq!(hub_tree(&active), imported);
            assert_eq!(hub_tree(&fixture.alpha.path("owned")), owned);
            assert_eq!(
                ["shared", "bot"].map(|repository| fixture.snapshots(&fixture.alpha, repository)),
                repositories
            );
        }
    }
    fs::write(&file, valid.to_string()).unwrap();
    fixture.ok(&fixture.alpha, &["status"]);
}
