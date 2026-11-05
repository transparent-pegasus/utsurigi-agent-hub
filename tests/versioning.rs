#[allow(dead_code)]
mod common;

use common::*;
use serde_json::json;
use std::fs;

fn disable_versioning(bot: &mut Bot) {
    bot.config["sync"]["versioning"] = json!(false);
    bot.save();
}

fn assert_no_cleanup(tool: &std::path::Path) {
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert!(!calls.lines().any(|call| matches!(call, "forget" | "prune")));
}

#[test]
fn disabled_versioning_keeps_the_new_verified_backup_and_can_be_reenabled() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "bot");
    write(
        fixture.alpha.path("owned").join("notes/new.md"),
        b"latest data",
    );
    fixture.ok(&fixture.alpha, &["sync"]);
    let current = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(current.len(), 1);
    assert!(current.is_disjoint(&old));
    let destination = fixture.temp.path().join("latest-restored");
    fixture.ok(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            current.first().unwrap(),
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fs::read(destination.join("notes/new.md")).unwrap(),
        b"latest data"
    );
    fixture.alpha.config["sync"]["versioning"] = json!(true);
    fixture.alpha.save();
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_eq!(fixture.ids(&fixture.alpha, "bot").len(), 2);
    assert!(current.is_subset(&fixture.ids(&fixture.alpha, "bot")));
}

#[test]
fn failed_or_unverified_backups_never_delete_predecessors() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "bot");
    for mode in ["network", "partial", "tag-failure"] {
        fs::write(tool.join("mode"), mode).unwrap();
        fs::write(tool.join("calls"), "").unwrap();
        failure(&fixture.cli(&fixture.alpha, &["sync"]));
        assert!(old.is_subset(&fixture.ids(&fixture.alpha, "bot")));
        assert_no_cleanup(&tool);
        // Each injected failure has been inspected before the next independent attempt.
        fs::remove_file(fixture.alpha.path("state").join("pending-backup.yaml")).unwrap();
    }
}

#[test]
fn publication_cleans_old_shared_versions_only_after_refreshing_the_cache() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "shared");
    let old_head = fixture.current_shared_id(&fixture.alpha);
    let bot_ids = fixture.ids(&fixture.alpha, "bot");
    write(
        fixture.alpha.path("shared").join("decision.md"),
        b"new policy",
    );
    fs::write(tool.join("calls"), "").unwrap();
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let current = fixture.ids(&fixture.alpha, "shared");
    assert_eq!(current.len(), old.len());
    assert!(!current.contains(&old_head));
    assert!(old.difference(&current).eq(std::iter::once(&old_head)));
    let active = fixture.active_shared(&fixture.alpha);
    assert_eq!(
        active.file_name().unwrap().to_str().unwrap(),
        fixture.current_shared_id(&fixture.alpha)
    );
    assert_eq!(fs::read(active.join("decision.md")).unwrap(), b"new policy");
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), bot_ids);
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    let calls: Vec<_> = calls.lines().collect();
    let cleanup = calls.iter().position(|call| *call == "forget").unwrap();
    assert!(calls.iter().position(|call| *call == "backup").unwrap() < cleanup);
    assert!(calls.iter().rposition(|call| *call == "restore").unwrap() < cleanup);
}

#[test]
fn failed_local_publication_retains_old_versions_and_the_retry_guard() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    let old = fixture.ids(&fixture.alpha, "shared");
    let active = fixture.active_shared(&fixture.alpha);
    fs::write(tool.join("mode"), "publish-import-failure").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["registry", "publish"]));
    let snapshots = fixture.ids(&fixture.alpha, "shared");
    assert_eq!(snapshots.len(), old.len() + 1);
    assert!(old.is_subset(&snapshots));
    assert_eq!(fixture.active_shared(&fixture.alpha), active);
    assert_no_cleanup(&tool);
    assert!(
        fixture
            .alpha
            .path("state")
            .join("pending-backup.yaml")
            .is_file()
    );
}

#[test]
fn failed_cleanup_keeps_the_verified_backup_and_blocks_another_write() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "bot");
    let state_path = fixture.alpha.path("state").join("status.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["anchor"] = json!(0);
    state["next_due"] = json!(0);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let status = fixture.status(&fixture.alpha);
    fs::write(tool.join("mode"), "forget-failure").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync", "--scheduled"]));
    let snapshots = fixture.snapshots(&fixture.alpha, "bot");
    assert_eq!(snapshots.len(), 2);
    assert!(old.is_subset(&fixture.ids(&fixture.alpha, "bot")));
    assert!(snapshots.iter().all(|snapshot| {
        snapshot["tags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
    }));
    assert_eq!(fixture.status(&fixture.alpha), status);
    assert!(
        fixture
            .alpha
            .path("state")
            .join("pending-backup.yaml")
            .is_file()
    );
    fs::write(tool.join("mode"), "pass").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_no_writes(&tool);
}

#[test]
fn incomplete_and_other_bot_snapshots_remain_available_for_inspection() {
    let mut fixture = Fixture::new();
    disable_versioning(&mut fixture.alpha);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "bot");
    success(&fixture.restic(
        &fixture.alpha,
        "bot",
        &[
            "backup",
            ".",
            "--host",
            "other",
            "--tag",
            "utsurigi-agent-hub",
            "--tag",
            "hub:default",
            "--tag",
            "kind:bot",
            "--tag",
            "bot:other",
            "--tag",
            "runtime:other-1",
            "--tag",
            "completed:1",
        ],
    ));
    let foreign = fixture
        .ids(&fixture.alpha, "bot")
        .difference(&old)
        .next()
        .unwrap()
        .clone();
    fs::write(tool.join("mode"), "partial").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    let incomplete = fixture
        .snapshots(&fixture.alpha, "bot")
        .into_iter()
        .find(|snapshot| {
            !snapshot["tags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tag| tag.as_str().unwrap().starts_with("completed:"))
        })
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(fixture.alpha.path("state").join("pending-backup.yaml")).unwrap();
    fs::write(tool.join("mode"), "pass").unwrap();
    fixture.ok(&fixture.alpha, &["sync"]);
    let remaining = fixture.ids(&fixture.alpha, "bot");
    assert_eq!(remaining.len(), 3);
    assert!(remaining.contains(&foreign) && remaining.contains(&incomplete));
    assert!(remaining.is_disjoint(&old));
    let destination = fixture.temp.path().join("incomplete-restored");
    fixture.ok(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            "bot",
            "--snapshot",
            &incomplete,
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
fn publication_replaces_the_prior_accepted_head_from_another_publisher() {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    disable_versioning(&mut beta);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&beta, &["sync"]);
    let old = fixture.ids(&fixture.alpha, "shared");
    let old_head = fixture.current_shared_id(&fixture.alpha);
    let bot_ids = fixture.ids(&beta, "bot");
    fixture.author_current(&beta);
    fixture.ok(&beta, &["registry", "publish"]);
    let current = fixture.ids(&beta, "shared");
    assert_eq!(current.len(), old.len());
    assert!(!current.contains(&old_head));
    assert!(old.difference(&current).eq(std::iter::once(&old_head)));
    assert_eq!(fixture.ids(&beta, "bot"), bot_ids);
    assert_eq!(
        fixture
            .active_shared(&beta)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        fixture.current_shared_id(&beta)
    );
}
