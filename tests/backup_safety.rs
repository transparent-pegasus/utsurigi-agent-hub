#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn restore_staging_is_private_before_restic_starts() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fs::write(tool.join("restic"), r#"#!/bin/sh
set -eu
operation=
target=
while [ "$#" -gt 0 ]; do
    case "$1" in
        version) printf '%s\n' 'restic 0.19.1'; exit 0 ;;
        snapshots|restore) operation=$1 ;;
        --target) shift; target=$1 ;;
    esac
    shift
done
if [ "$operation" = snapshots ]; then
    printf '[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","tags":["hub:default"]}]\n'
elif [ "$operation" = restore ]; then
    mode=$(ls -ld -- "$target")
    case "$mode" in drwx------*) ;; *) printf '%s\n' 'staging is not private' >&2; exit 73 ;; esac
    printf '%s\n' inspected > "$target/payload"
else
    exit 9
fi
"#).unwrap();
    let destination = fixture.temp.path().join("restored");
    let output = fixture.cli(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            "aaaaaaaa",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    success(&output);
    assert_eq!(
        fs::read_to_string(destination.join("payload")).unwrap(),
        "inspected\n"
    );
    assert_eq!(
        fs::metadata(destination).unwrap().permissions().mode() & 0o777,
        0o700
    );
}

#[test]
fn no_write_guard_rejects_snapshot_tagging() {
    let temp = TempDir::new();
    fs::write(temp.path().join("calls"), "tag\n").unwrap();
    assert!(std::panic::catch_unwind(|| assert_no_writes(temp.path())).is_err());
}

#[test]
fn failed_backup_blocks_a_second_process_until_reconciled() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(tool.join("mode"), "partial").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    let before = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(before.len(), 1);
    fs::write(tool.join("mode"), "pass").unwrap();
    let second = fixture.cli(&fixture.alpha, &["sync"]);
    failure(&second);
    assert!(text(&second).contains("reconcil"), "{}", text(&second));
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "backup").count(), 1);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), before);
    let journal = fixture.alpha.path("state").join("pending-backup.yaml");
    let record: Value = serde_saphyr::from_str(&fs::read_to_string(&journal).unwrap()).unwrap();
    assert_eq!(record["hub"], "default");
    assert_eq!(record["repository"], fixture.alpha.repository("bot"));
    assert_eq!(
        fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Inspection remains possible and must leave the write guard intact.
    let inspected = fixture.temp.path().join("incomplete-inspected");
    fixture.ok(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            before.first().unwrap(),
            "--destination",
            inspected.to_str().unwrap(),
        ],
    );
    assert!(journal.is_file());
    // Explicit host reconciliation retains the old snapshot and clears only this guard.
    fs::remove_file(journal).unwrap();
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot").len(), 2);
}

#[test]
fn publication_updates_the_verified_shared_cache_immediately() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let first = fixture.active_shared(&fixture.alpha);
    write(
        fixture.alpha.path("shared").join("decision.md"),
        b"new shared decision",
    );
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let active = fixture.active_shared(&fixture.alpha);
    assert_ne!(active, first);
    assert_eq!(
        fs::read(active.join("decision.md")).unwrap(),
        b"new shared decision"
    );
}

#[test]
fn empty_password_references_match_null_registry_references() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["repositories"]["bot"]["password_ref"] = json!("");
    fixture.alpha.save();
    let registry_path = fixture.alpha.path("shared").join("registry.yaml");
    let mut registry: Value = serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    registry["bots"][0]["password_ref"] = Value::Null;
    fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
}

#[test]
fn peer_identity_mismatch_requires_an_explicit_pull_option() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let id = fixture.ids(&fixture.alpha, "bot").first().unwrap().clone();
    // Even when an expected tag remains, a conflicting runtime is not silently accepted.
    success(&fixture.restic(
        &fixture.alpha,
        "bot",
        &["tag", "--add", "runtime:other-runtime", &id],
    ));
    let destination = fixture.temp.path().join("mismatched-peer");
    let denied = fixture.cli(
        &fixture.alpha,
        &[
            "pull",
            "alpha",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    failure(&denied);
    assert!(
        text(&denied).contains("--allow-identity-mismatch"),
        "{}",
        text(&denied)
    );
    assert!(!destination.exists());
    fixture.ok(
        &fixture.alpha,
        &[
            "pull",
            "alpha",
            "--allow-identity-mismatch",
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
}

#[test]
fn missing_source_error_names_the_requested_path() {
    let fixture = Fixture::new();
    let owned = fixture.alpha.path("owned");
    fs::remove_dir_all(&owned).unwrap();
    let output = fixture.cli(&fixture.alpha, &["setup"]);
    failure(&output);
    assert!(
        text(&output).contains(owned.to_str().unwrap()),
        "{}",
        text(&output)
    );
}
#[test]
fn verified_publication_with_failed_local_import_requires_reconciliation() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    let active = fixture.active_shared(&fixture.alpha);
    fs::write(tool.join("mode"), "publish-import-failure").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["registry", "publish"]));
    let snapshots = fixture.ids(&fixture.alpha, "shared");
    assert_eq!(fixture.content_snapshots(&fixture.alpha, "shared").len(), 2);
    assert_eq!(fixture.active_shared(&fixture.alpha), active);
    let output = fixture.cli(&fixture.alpha, &["status"]);
    success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["next_action"],
        "reconcile"
    );
    fs::write(tool.join("mode"), "pass").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    for args in [
        &["registry", "publish"][..],
        &["sync"],
        &["setup"],
        &["prune"],
    ] {
        failure(&fixture.cli(&fixture.alpha, args));
        assert_no_writes(&tool);
    }
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), snapshots);
}

#[test]
fn rejected_backup_preflight_does_not_leave_a_write_attempt() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    success(&fixture.restic(
        &fixture.alpha,
        "bot",
        &["backup", ".", "--tag", "hub:other"],
    ));
    fs::write(tool.join("calls"), "").unwrap();
    let rejected = fixture.cli(&fixture.alpha, &["sync"]);
    failure(&rejected);
    assert!(text(&rejected).contains("Snapshot hub"));
    assert_no_writes(&tool);
    assert!(
        !fixture
            .alpha
            .path("state")
            .join("pending-backup.yaml")
            .exists()
    );
}
