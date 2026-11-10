#[allow(dead_code)]
mod common;
use common::*;
use serde_json::{Value, json};
use std::fs;

fn edit(fixture: &Fixture, bot: &Bot, id: &str, status: &str) {
    let output = fixture.ok(bot, &["tasks", "show", id]);
    let old: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut task = old["task"].clone();
    task["status"] = json!(status);
    let path = fixture.temp.path().join("task-input.md");
    fs::write(
        &path,
        format!("---\n{}\n---\n{}", task, old["body"].as_str().unwrap()),
    )
    .unwrap();
    fixture.ok(
        bot,
        &[
            "tasks",
            "put",
            "--file",
            path.to_str().unwrap(),
            "--if-match",
            old["etag"].as_str().unwrap(),
        ],
    );
}

#[test]
fn shared_tasks_publish_between_bots_archive_completion_and_reject_deleted_history() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["tasks", "init"]);
    let created: Value = serde_json::from_slice(
        &fixture
            .ok(alpha, &["tasks", "create", "shared work"])
            .stdout,
    )
    .unwrap();
    let id = created["task"]["id"].as_str().unwrap();
    edit(&fixture, alpha, id, "in_progress");
    fixture.ok(alpha, &["setup"]);
    let beta = fixture.bot("beta", "beta-1");
    fixture.ok(&beta, &["setup"]);
    let accepted: Value =
        serde_json::from_slice(&fixture.ok(&beta, &["tasks", "list", "--accepted"]).stdout)
            .unwrap();
    assert_eq!(accepted[0]["task"]["id"], id);
    fixture.ok(&beta, &["tasks", "show", id, "--accepted"]);
    fixture.author_current(&beta);
    let rows: Value =
        serde_json::from_slice(&fixture.ok(&beta, &["tasks", "list"]).stdout).unwrap();
    assert_eq!(rows[0]["task"]["id"], id);
    edit(&fixture, &beta, id, "completed");
    fixture.ok(&beta, &["registry", "publish"]);
    fixture.ok(alpha, &["sync"]);
    let active = fixture.active_shared(alpha);
    assert!(!active.join("tasks").join(format!("{id}.md")).exists());
    assert!(
        active
            .join("tasks/archive")
            .join(format!("{id}.md"))
            .exists()
    );
    assert!(
        !fs::read_to_string(active.join("tasks/INDEX.md"))
            .unwrap()
            .contains(id)
    );
    fixture.author_current(alpha);
    fs::remove_file(
        alpha
            .path("shared")
            .join("tasks/archive")
            .join(format!("{id}.md")),
    )
    .unwrap();
    let before = fixture.current_shared_id(alpha);
    let output = fixture.cli(alpha, &["registry", "publish"]);
    failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Retain"));
    assert_eq!(fixture.current_shared_id(alpha), before);
}

#[test]
fn publication_rejects_malformed_task_graphs_and_stale_index_before_acceptance() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["tasks", "init"]);
    let created: Value =
        serde_json::from_slice(&fixture.ok(alpha, &["tasks", "create", "work"]).stdout).unwrap();
    let id = created["task"]["id"].as_str().unwrap();
    edit(&fixture, alpha, id, "in_progress");
    fixture.ok(alpha, &["setup"]);
    fixture.author_current(alpha);
    let before = fixture.current_shared_id(alpha);
    let path = alpha.path("shared").join("tasks").join(format!("{id}.md"));
    let original = fs::read(&path).unwrap();
    let mut task = created["task"].clone();
    task["status"] = json!("in_progress");
    task["start_after"] = json!(["00000000-0000-4000-8000-000000000001"]);
    fs::write(&path, format!("---\n{task}\n---\nbody")).unwrap();
    let output = fixture.cli(alpha, &["registry", "publish"]);
    failure(&output);
    assert_eq!(fixture.current_shared_id(alpha), before);
    fs::write(&path, original).unwrap();
    fs::write(alpha.path("shared").join("tasks/INDEX.md"), "stale").unwrap();
    let output = fixture.cli(alpha, &["registry", "publish"]);
    failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("INDEX"));
    assert_eq!(fixture.current_shared_id(alpha), before);
}

#[test]
fn setup_preserves_unmarked_native_tasks_and_requires_migration() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    let tasks = alpha.path("shared").join("tasks");
    fs::remove_dir_all(&tasks).unwrap();
    fs::create_dir(&tasks).unwrap();
    fs::write(
        tasks.join("native.md"),
        "native format, deliberately no YAML",
    )
    .unwrap();
    let output = fixture.cli(alpha, &["setup"]);
    failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("required"));
    assert_eq!(
        fs::read_to_string(tasks.join("native.md")).unwrap(),
        "native format, deliberately no YAML"
    );
}

#[test]
fn setup_requires_migration_even_when_legacy_configuration_disabled_tasks() {
    let mut fixture = Fixture::new();
    let tasks = fixture.alpha.path("shared").join("tasks");
    if tasks.exists() {
        fs::remove_dir_all(&tasks).unwrap();
    }
    fixture.alpha.config["tasks"] = json!({"enabled":false});
    fixture.alpha.save();
    let output = fixture.cli(&fixture.alpha, &["setup"]);
    failure(&output);
    assert!(String::from_utf8_lossy(&output.stderr).contains("required"));
    fixture.ok(&fixture.alpha, &["tasks", "init"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    assert!(
        fixture
            .active_shared(&fixture.alpha)
            .join("tasks/FORMAT")
            .exists()
    );
}
