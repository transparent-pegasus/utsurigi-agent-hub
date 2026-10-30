#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

fn instrument(fixture: &mut Fixture) -> PathBuf {
    let directory = Fixture::wrapper(&mut fixture.alpha);
    fs::rename(directory.join("restic"), directory.join("logged-restic")).unwrap();
    fs::write(directory.join("summary-mode"), "pass").unwrap();
    fs::write(directory.join("observed"), "").unwrap();
    fs::write(
        directory.join("restic"),
        r#"#!/bin/sh
set -eu
directory=${0%/*}
operation=other
repository=
previous=
snapshot_object=false
for argument do
    if [ "$previous" = --repo ]; then repository=$argument; fi
    if [ "$previous" = cat ] && [ "$argument" = snapshot ]; then snapshot_object=true; fi
    case "$argument" in
        version|init|snapshots|backup|cat|tag|restore|forget|prune|unlock) operation=$argument ;;
    esac
    previous=$argument
done
{
    printf '%s\n' __CALL__
    for argument do printf 'arg:%s\n' "$argument"; done
} >> "$directory/observed"
mode=$(cat "$directory/summary-mode")
if [ "$operation" = tag ] && [ "$mode" = tag-noop ]; then exit 0; fi
if [ "$operation" = cat ] && [ "$snapshot_object" = true ] && [ "$mode" = metadata ]; then
    "$directory/logged-restic" "$@" > "$directory/snapshot-stdout"
    sed -f "$directory/mutation" "$directory/snapshot-stdout"
    exit 0
fi
if [ "$operation" = backup ]; then
    code=0
    "$directory/logged-restic" "$@" > "$directory/backup-stdout" || code=$?
    if [ "$code" != 0 ]; then cat "$directory/backup-stdout"; exit "$code"; fi
    case "$mode" in
        replace) cat "$directory/replacement" ;;
        duplicate) cat "$directory/backup-stdout" "$directory/backup-stdout" ;;
        foreign-after)
            "$directory/real-restic" --repo "$repository" --cache-dir "$directory/foreign-cache" backup . --json --host beta --tag utsurigi-agent-hub --tag hub:default --tag bot:beta --tag runtime:beta-1 --tag kind:bot --tag run:foreign > "$directory/foreign-stdout"
            cat "$directory/foreign-stdout"
            ;;
        *) cat "$directory/backup-stdout" ;;
    esac
    exit 0
fi
exec "$directory/logged-restic" "$@"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory.join("restic"), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn calls(directory: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(directory.join("observed"))
        .unwrap()
        .split("__CALL__\n")
        .skip(1)
        .map(|call| {
            call.lines()
                .filter_map(|line| line.strip_prefix("arg:").map(str::to_owned))
                .collect()
        })
        .collect()
}

fn operation(args: &[String]) -> &str {
    args.iter()
        .find(|arg| {
            [
                "version",
                "init",
                "snapshots",
                "backup",
                "cat",
                "tag",
                "restore",
            ]
            .contains(&arg.as_str())
        })
        .map(String::as_str)
        .unwrap_or("other")
}

fn assert_incomplete(fixture: &Fixture, directory: &Path, before: &Value) {
    let observed = calls(directory);
    assert_eq!(
        observed
            .iter()
            .filter(|args| operation(args) == "backup")
            .count(),
        1
    );
    assert!(!observed.iter().any(|args| operation(args) == "tag"));
    assert_eq!(fixture.status(&fixture.alpha), *before);
    let snapshots = fixture.snapshots(&fixture.alpha, "bot");
    assert!(!snapshots.is_empty(), "the real write must have happened");
    assert!(snapshots.iter().all(|snapshot| {
        snapshot["tags"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tag| !tag.as_str().unwrap().starts_with("completed:"))
    }));
}

#[test]
fn backup_uses_summary_id_and_only_repository_reads_skip_locks() {
    let mut fixture = Fixture::new();
    let directory = instrument(&mut fixture);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(directory.join("observed"), "").unwrap();
    fixture.ok(&fixture.alpha, &["sync"]);
    let stdout = fs::read(directory.join("backup-stdout")).unwrap();
    let summary = serde_json::Deserializer::from_slice(&stdout)
        .into_iter::<Value>()
        .map(Result::unwrap)
        .find(|message| message["message_type"] == "summary")
        .unwrap();
    let observed = calls(&directory);
    let backup = observed
        .iter()
        .position(|args| operation(args) == "backup")
        .unwrap();
    let tag = observed
        .iter()
        .position(|args| operation(args) == "tag")
        .unwrap();
    assert!(
        !observed[backup + 1..tag]
            .iter()
            .any(|args| operation(args) == "snapshots"),
        "the summary must replace the intermediate snapshot listing"
    );
    assert_eq!(
        observed[tag].last().unwrap(),
        summary["snapshot_id"].as_str().unwrap()
    );
    let mut reads = 0;
    let mut restores = 0;
    for args in &observed {
        let op = operation(args);
        if ["snapshots", "cat"].contains(&op) {
            reads += 1;
            assert!(
                args.iter().any(|arg| arg == "--no-lock"),
                "read must skip repository locks: {args:?}"
            );
        } else {
            assert!(
                !args.iter().any(|arg| arg == "--no-lock"),
                "{op} must retain its locking behavior"
            );
        }
        if op == "restore" {
            restores += 1;
            assert!(args.iter().any(|arg| arg == "--verify"));
        }
    }
    assert!(reads > 0);
    assert!(restores > 0);
    let snapshots = fixture.snapshots(&fixture.alpha, "bot");
    assert_eq!(snapshots.len(), 1);
    assert!(
        snapshots[0]["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
    );
}

#[test]
fn malformed_backup_json_never_completes_or_replays_a_write() {
    let mut fixture = Fixture::new();
    let directory = instrument(&mut fixture);
    fixture.ok(&fixture.alpha, &["setup"]);
    let before = fixture.status(&fixture.alpha);
    fs::write(directory.join("summary-mode"), "replace").unwrap();
    for payload in [
        String::new(),
        "not json".into(),
        json!({"message_type": "summary"}).to_string(),
        json!({"message_type": "summary", "snapshot_id": "abcd"}).to_string(),
        json!({"message_type": "summary", "snapshot_id": "g".repeat(64)}).to_string(),
        json!({"message_type": "summary", "snapshot_id": 123}).to_string(),
        json!({"message_type": "status", "snapshot_id": "a".repeat(64)}).to_string(),
        format!(
            "{{\"message_type\":\"summary\",\"snapshot_id\":\"{}\",\"snapshot_id\":\"{}\"}}",
            "a".repeat(64),
            "b".repeat(64)
        ),
    ] {
        fs::write(directory.join("replacement"), payload).unwrap();
        fs::write(directory.join("observed"), "").unwrap();
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_incomplete(&fixture, &directory, &before);
    }
    fs::write(directory.join("summary-mode"), "duplicate").unwrap();
    fs::write(directory.join("observed"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_incomplete(&fixture, &directory, &before);
}

#[test]
fn foreign_summary_created_after_preflight_cannot_receive_completion() {
    let mut fixture = Fixture::new();
    let directory = instrument(&mut fixture);
    fixture.ok(&fixture.alpha, &["setup"]);
    let before = fixture.status(&fixture.alpha);
    fs::write(directory.join("summary-mode"), "foreign-after").unwrap();
    fs::write(directory.join("observed"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_incomplete(&fixture, &directory, &before);
    assert!(
        fixture
            .snapshots(&fixture.alpha, "bot")
            .iter()
            .any(|snapshot| snapshot["hostname"] == "beta")
    );
}

#[test]
fn summary_snapshot_metadata_must_match_the_current_write() {
    let mut fixture = Fixture::new();
    let directory = instrument(&mut fixture);
    fixture.ok(&fixture.alpha, &["setup"]);
    let before = fixture.status(&fixture.alpha);
    fs::write(directory.join("summary-mode"), "metadata").unwrap();
    for mutation in [
        "s/\"hostname\": \"alpha\"/\"hostname\": \"beta\"/",
        "s/\"hub:default\"/\"hub:foreign\"/",
        "s/\"bot:alpha\"/\"bot:beta\"/",
        "s/\"kind:bot\"/\"kind:shared\"/",
        "s/\"run:[^\"]*\"/\"run:foreign\"/",
        "s/\"runtime:alpha-1\"/\"runtime:foreign\"/",
        "s/\"utsurigi-agent-hub\"/\"unrelated\"/",
    ] {
        fs::write(directory.join("mutation"), mutation).unwrap();
        fs::write(directory.join("observed"), "").unwrap();
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert_incomplete(&fixture, &directory, &before);
    }
}

#[test]
fn successful_tag_exit_requires_completion_reconciliation() {
    let mut fixture = Fixture::new();
    let directory = instrument(&mut fixture);
    fixture.ok(&fixture.alpha, &["setup"]);
    let before = fixture.status(&fixture.alpha);
    fs::write(directory.join("summary-mode"), "tag-noop").unwrap();
    fs::write(directory.join("observed"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_eq!(fixture.status(&fixture.alpha), before);
    let observed = calls(&directory);
    assert_eq!(
        observed
            .iter()
            .filter(|args| operation(args) == "backup")
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|args| operation(args) == "tag")
            .count(),
        1
    );
    assert!(
        observed
            .last()
            .is_some_and(|args| operation(args) == "snapshots")
    );
}
