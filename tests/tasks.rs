use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const A: &str = "00000000-0000-4000-8000-000000000001";
const B: &str = "00000000-0000-4000-8000-000000000002";
const C: &str = "00000000-0000-4000-8000-000000000003";

struct Case {
    temp: tempfile::TempDir,
    config: PathBuf,
    shared: PathBuf,
}

fn command(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_utsurigi-agent-hub"))
        .env_clear()
        .arg("--config")
        .arg(config)
        .arg("tasks")
        .args(args)
        .output()
        .unwrap()
}

fn ok(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn bad(output: Output, message: &str) {
    assert!(!output.status.success(), "unexpected success: {:?}", output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn task(id: &str) -> Value {
    json!({"id":id,"parent":null,"title":"導入 🐎","status":"pending",
        "owner":"alpha","schedule":null,"due":"2026-11-15T23:59+09:00",
        "workdir":"/workspace/intro","requires":[{"repo":"owner/repo"},{"secret":"vault-item"}],
        "start_after":[],"done_when":[],"updated":"2026-11-10T05:21+09:00"})
}

fn markdown(metadata: &Value, body: &str) -> String {
    format!(
        "---\n{}\n---\n{body}",
        serde_json::to_string_pretty(metadata).unwrap()
    )
}

impl Case {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.json");
        let shared = temp.path().join("shared");
        fs::write(
            &config,
            json!({"bot":{"id":"alpha"},"paths":{"shared":shared}}).to_string(),
        )
        .unwrap();
        Self {
            temp,
            config,
            shared,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        command(&self.config, args)
    }
    fn init(&self) {
        ok(self.run(&["init"]));
    }
    fn get(&self, id: &str) -> Value {
        ok(self.run(&["show", id]))
    }
    fn path(&self, id: &str) -> PathBuf {
        let archived = self.shared.join("tasks/archive").join(format!("{id}.md"));
        if archived.exists() {
            archived
        } else {
            self.shared.join("tasks").join(format!("{id}.md"))
        }
    }
    fn input(&self, name: &str, metadata: &Value, body: &str) -> PathBuf {
        let path = self.temp.path().join(name);
        fs::write(&path, markdown(metadata, body)).unwrap();
        path
    }
    fn put(&self, metadata: &Value, body: &str, etag: Option<&str>) -> Output {
        let input = self.input("input.md", metadata, body);
        let mut args = vec!["put", "--file", input.to_str().unwrap()];
        if let Some(etag) = etag {
            args.extend(["--if-match", etag]);
        }
        self.run(&args)
    }
    fn update(&self, id: &str, status: &str) -> Output {
        let old = self.get(id);
        let mut metadata = old["task"].clone();
        metadata["status"] = json!(status);
        self.put(
            &metadata,
            old["body"].as_str().unwrap(),
            old["etag"].as_str(),
        )
    }
    fn import(&self, format: &str, records: Value) -> Output {
        let file = self.temp.path().join("native.json");
        fs::write(&file, records.to_string()).unwrap();
        self.run(&["import", format, "--file", file.to_str().unwrap()])
    }
}

#[test]
fn task_migration_preserves_native_files_and_legacy_false_cannot_disable_tasks() {
    let c = Case::new();
    fs::create_dir_all(c.shared.join("tasks")).unwrap();
    fs::write(c.shared.join("tasks/native.md"), "native checklist").unwrap();
    bad(c.run(&["init"]), "existing");
    assert_eq!(
        fs::read_to_string(c.shared.join("tasks/native.md")).unwrap(),
        "native checklist"
    );
    let preserved = c.temp.path().join("native-tasks");
    fs::rename(c.shared.join("tasks"), &preserved).unwrap();
    fs::write(
        &c.config,
        json!({"bot":{"id":"alpha"},"tasks":{"enabled":false},"paths":{"shared":c.shared}})
            .to_string(),
    )
    .unwrap();
    ok(c.run(&["init"]));
    ok(c.run(&["create", "mandatory work"]));
    assert!(c.shared.join("tasks/FORMAT").exists());
    assert_eq!(
        fs::read_to_string(preserved.join("native.md")).unwrap(),
        "native checklist"
    );
}

#[test]
fn accepted_progress_reads_require_a_verified_shared_cache() {
    let c = Case::new();
    c.init();
    bad(c.run(&["list", "--accepted"]), "verified shared");
    bad(c.run(&["show", A, "--accepted"]), "verified shared");
}

#[test]
fn tasks_round_trip_the_example_and_retain_completed_files_outside_index() {
    let c = Case::new();
    c.init();
    let body = "## Next\n承認をもらう\n\n## Note\nregistry rev8、全員shared。\n";
    let saved = ok(c.put(&task(A), body, None));
    assert!(saved["body"].as_str().unwrap().starts_with(body));
    assert!(saved["body"].as_str().unwrap().contains("| alpha |"));
    assert_eq!(saved["task"]["requires"], task(A)["requires"]);
    assert_eq!(saved["task"]["due"], task(A)["due"]);
    assert_eq!(saved["etag"].as_str().unwrap().len(), 64);
    let mut metadata = saved["task"].clone();
    metadata["status"] = json!("completed");
    ok(c.put(&metadata, body, saved["etag"].as_str()));
    assert!(c.path(A).exists());
    assert!(
        !fs::read_to_string(c.shared.join("tasks/INDEX.md"))
            .unwrap()
            .contains(A)
    );
    assert_eq!(ok(c.run(&["list"])), json!([]));
    assert_eq!(ok(c.run(&["list", "--all"])).as_array().unwrap().len(), 1);
    assert_eq!(ok(c.run(&["validate"]))["valid"], true);
}

#[test]
fn create_generates_a_uuid_and_closed_tasks_reopen_through_pending() {
    let c = Case::new();
    c.init();
    let new = ok(c.run(&["create", "new work"]));
    let id = new["task"]["id"].as_str().unwrap();
    assert_eq!(id.len(), 36);
    assert!(c.path(id).exists());
    ok(c.update(id, "waiting"));
    ok(c.update(id, "cancelled"));
    bad(c.update(id, "in_progress"), "pending");
    ok(c.update(id, "pending"));
    ok(c.update(id, "in_progress"));
}

#[test]
fn user_work_waits_have_their_own_status_and_templates_include_a_log_after_note() {
    let c = Case::new();
    c.init();
    let new = ok(c.run(&["create", "need user action"]));
    let id = new["task"]["id"].as_str().unwrap();
    assert!(
        new["body"]
            .as_str()
            .unwrap()
            .contains("## Note\n\n## Log\n")
    );
    ok(c.update(id, "waiting_user"));
    assert_eq!(c.get(id)["ready"], false);
    assert_eq!(
        ok(c.run(&["list", "--status", "waiting_user"]))[0]["task"]["id"],
        id
    );
    assert_eq!(ok(c.run(&["list"])), json!([]));
    let imported = ok(c.import("muse",json!([{"id":"user","kind":"tracking","title":"user input","status":"waiting_user","note":"details","activity":"2026-11-11: asked"}])));
    let got = c.get(imported["ids"]["user"].as_str().unwrap());
    assert!(got["body"].as_str().unwrap().contains("| alpha |"));
    assert!(got["body"].as_str().unwrap().contains("2026-11-11: asked"));
    assert!(
        got["body"]
            .as_str()
            .unwrap()
            .contains("## Note\ndetails\n\n## Log\n")
    );
}

#[test]
fn stale_etags_and_duplicate_creates_preserve_the_winning_task() {
    let c = Case::new();
    c.init();
    let saved = ok(c.put(&task(A), "original", None));
    bad(c.put(&task(A), "overwrite", None), "if-match");
    ok(c.put(&task(A), "winner", saved["etag"].as_str()));
    bad(c.put(&task(A), "stale", saved["etag"].as_str()), "conflict");
    assert!(c.get(A)["body"].as_str().unwrap().starts_with("winner"));
}

#[test]
fn independent_bot_writers_have_exactly_one_etag_winner() {
    let c = Case::new();
    c.init();
    let old = ok(c.put(&task(A), "base", None));
    let paths = [
        c.input("one.md", &task(A), "one"),
        c.input("two.md", &task(A), "two"),
    ];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = paths
        .into_iter()
        .map(|path| {
            let config = c.config.clone();
            let etag = old["etag"].as_str().unwrap().to_owned();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                command(
                    &config,
                    &["put", "--file", path.to_str().unwrap(), "--if-match", &etag],
                )
            })
        })
        .collect();
    let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(outputs.iter().filter(|o| o.status.success()).count(), 1);
    let body = c.get(A)["body"].as_str().unwrap().to_owned();
    assert!(body.starts_with("one") || body.starts_with("two"));
}

#[test]
fn malformed_metadata_and_missing_references_never_write_a_task() {
    let c = Case::new();
    c.init();
    for (field, value) in [
        ("id", json!("../../escape")),
        ("status", json!("forgotten")),
        ("updated", json!("yesterday")),
        ("due", json!("2026-11-15T23:59")),
        ("parent", json!(B)),
        ("start_after", json!([B])),
        ("done_when", json!([B])),
        ("requires", json!([{"secret":""}])),
        ("schedule", json!({"bot":"alpha","id":""})),
        ("unexpected", json!(true)),
    ] {
        let mut metadata = task(A);
        metadata[field] = value;
        assert!(
            !c.put(&metadata, "", None).status.success(),
            "accepted {field}"
        );
        assert!(!c.path(A).exists());
    }
}

#[test]
fn dependency_and_parent_cycles_are_separate_and_cross_kind_wait_cycles_fail() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "", None));
    let mut child = task(B);
    child["parent"] = json!(A);
    ok(c.put(&child, "", None));
    let old = c.get(A);
    let mut parent = old["task"].clone();
    parent["done_when"] = json!([B]);
    ok(c.put(&parent, "", old["etag"].as_str()));
    let old = c.get(B);
    child["start_after"] = json!([A]);
    bad(c.put(&child, "", old["etag"].as_str()), "cycle");
    let old = c.get(A);
    parent["parent"] = json!(B);
    bad(c.put(&parent, "", old["etag"].as_str()), "cycle");
}

#[test]
fn start_and_completion_dependencies_gate_their_own_transitions() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "", None));
    ok(c.put(&task(C), "", None));
    let mut dependent = task(B);
    dependent["start_after"] = json!([A]);
    dependent["done_when"] = json!([C]);
    ok(c.put(&dependent, "", None));
    bad(c.update(B, "in_progress"), "blocked");
    ok(c.update(A, "completed"));
    ok(c.update(B, "in_progress"));
    bad(c.update(B, "completed"), "done_when");
    ok(c.update(C, "completed"));
    ok(c.update(B, "completed"));
}

#[test]
fn cancellation_blocks_descendants_and_dependants_without_rewriting_history() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "", None));
    let mut child = task(B);
    child["parent"] = json!(A);
    ok(c.put(&child, "", None));
    let mut dependent = task(C);
    dependent["start_after"] = json!([A]);
    ok(c.put(&dependent, "", None));
    ok(c.update(A, "cancelled"));
    assert_eq!(c.get(B)["task"]["status"], "cancelled");
    let got = c.get(C);
    assert_eq!(got["task"]["status"], "pending");
    assert_eq!(got["ready"], false);
    assert!(!got["blockers"].as_array().unwrap().is_empty());
    bad(c.update(C, "completed"), "blocked");
    ok(c.update(A, "pending"));
    assert_eq!(c.get(B)["task"]["status"], "cancelled");
}

#[test]
fn cancelling_a_parent_preserves_already_completed_child_history() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "", None));
    let mut child = task(B);
    child["parent"] = json!(A);
    child["status"] = json!("completed");
    ok(c.put(&child, "finished", None));
    ok(c.update(A, "cancelled"));
    assert_eq!(c.get(B)["task"]["status"], "completed");
}

#[test]
fn pending_batches_recover_before_reads_and_do_not_drop_body_text() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "old", None));
    let mut parent = task(A);
    parent["done_when"] = json!([B]);
    let mut child = task(B);
    child["parent"] = json!(A);
    let documents = json!({A:markdown(&parent,"## Goal\nlong goal\n"),B:markdown(&child,"next")});
    let journal = c.shared.join("tasks/.pending.json");
    fs::write(
        &journal,
        json!({"version":1,"documents":documents}).to_string(),
    )
    .unwrap();
    assert_eq!(ok(c.run(&["list", "--all"])).as_array().unwrap().len(), 2);
    assert_eq!(c.get(A)["body"], "## Goal\nlong goal\n");
    assert!(!journal.exists());
    fs::write(&journal, "not valid JSON").unwrap();
    bad(c.run(&["list"]), "journal");
    assert!(journal.exists());
}

#[test]
fn index_can_be_rebuilt_after_manual_edits_and_escapes_task_titles() {
    let c = Case::new();
    c.init();
    let mut metadata = task(A);
    metadata["title"] = json!("[title](test)");
    metadata["status"] = json!("in_progress");
    ok(c.put(&metadata, "", None));
    let index = fs::read_to_string(c.shared.join("tasks/INDEX.md")).unwrap();
    assert!(index.contains("\\[title\\]"));
    ok(c.update(A, "completed"));
    fs::write(c.shared.join("tasks/INDEX.md"), "stale").unwrap();
    ok(c.run(&["index"]));
    assert!(
        !fs::read_to_string(c.shared.join("tasks/INDEX.md"))
            .unwrap()
            .contains(A)
    );
}

#[test]
fn progress_reads_return_only_in_progress_and_do_not_read_archived_bodies() {
    let c = Case::new();
    c.init();
    for (id, status) in [
        (A, "in_progress"),
        (B, "pending"),
        (C, "waiting"),
        ("00000000-0000-4000-8000-000000000004", "completed"),
    ] {
        let mut metadata = task(id);
        metadata["status"] = json!(status);
        ok(c.put(&metadata, "body", None));
    }
    let archived = c.path("00000000-0000-4000-8000-000000000004");
    assert!(archived.starts_with(c.shared.join("tasks/archive")));
    fs::write(&archived, "deliberately unreadable as a task").unwrap();
    let progress = ok(c.run(&["list"]));
    assert_eq!(progress.as_array().unwrap().len(), 1);
    assert_eq!(progress[0]["task"]["id"], A);
    assert_eq!(
        ok(c.run(&["list", "--status", "pending"]))[0]["task"]["id"],
        B
    );
    fs::write(c.path(B), "not parsed during a progress read").unwrap();
    assert_eq!(ok(c.run(&["list"])).as_array().unwrap().len(), 1);
    let index = fs::read_to_string(c.shared.join("tasks/INDEX.md")).unwrap();
    assert!(index.contains(A));
    assert!(!index.contains(B));
    assert!(!index.contains(C));
    assert!(!c.run(&["list", "--all"]).status.success());
}

#[test]
fn symlinks_and_mismatched_task_filenames_are_rejected() {
    use std::os::unix::fs::symlink;
    let c = Case::new();
    c.init();
    let target = c.temp.path().join("outside");
    fs::write(&target, "keep").unwrap();
    symlink(&target, c.path(A)).unwrap();
    bad(c.run(&["list", "--all"]), "Symlink");
    assert_eq!(fs::read_to_string(target).unwrap(), "keep");
    fs::remove_file(c.path(A)).unwrap();
    fs::write(c.path(B), markdown(&task(A), "")).unwrap();
    bad(c.run(&["list", "--all"]), "filename");
}

#[test]
fn grok_import_is_scoped_idempotent_and_preserves_shared_edits_and_closed_history() {
    let c = Case::new();
    c.init();
    let records = json!([{"id":"1","content":"first","status":"pending"},{"id":"2","content":"done","status":"completed"}]);
    let result = ok(c.import("grok", records.clone()));
    assert_eq!(result["imported"], 2);
    let id = result["ids"]["1"].as_str().unwrap();
    let old = c.get(id);
    ok(c.put(&old["task"], "shared edit", old["etag"].as_str()));
    assert_eq!(ok(c.import("grok", records.clone()))["imported"], 0);
    assert!(
        c.get(id)["body"]
            .as_str()
            .unwrap()
            .starts_with("shared edit")
    );
    fs::write(
        &c.config,
        json!({"bot":{"id":"beta"},"paths":{"shared":c.shared}}).to_string(),
    )
    .unwrap();
    let second = ok(c.import("grok", records));
    assert_eq!(second["imported"], 2);
    assert_ne!(second["ids"]["1"], id);
    assert_eq!(ok(c.run(&["list", "--all"])).as_array().unwrap().len(), 4);
}

#[test]
fn muse_import_preserves_goals_logs_schedule_refs_and_maps_all_dependencies() {
    let c = Case::new();
    c.init();
    let result = ok(c.import("muse",json!([
        {"id":"goal","kind":"goal","title":"成果物","status":"in_progress","workdir":"/workspace/goal","done_when":["child"],"goal":"GOAL.md の本文","activity":"2026-11-10: began"},
        {"id":"child","kind":"checklist","title":"作業","status":"pending","parent":"goal","next":"review","note":"retain"},
        {"id":"tracking","kind":"tracking","title":"配送待ち","status":"waiting","start_after":["child"],"due":"2026-11-15T23:59+09:00"},
        {"id":"routine","kind":"schedule","title":"定期確認","status":"pending","parent":"goal","schedule":"native-cron-12"}
    ])));
    let ids = &result["ids"];
    let goal = c.get(ids["goal"].as_str().unwrap());
    assert!(goal["body"].as_str().unwrap().contains("GOAL.md の本文"));
    assert!(goal["body"].as_str().unwrap().contains("2026-11-10: began"));
    assert_eq!(goal["task"]["workdir"], "/workspace/goal");
    assert_eq!(goal["task"]["done_when"][0], ids["child"]);
    let routine = c.get(ids["routine"].as_str().unwrap());
    assert_eq!(
        routine["task"]["schedule"],
        json!({"bot":"alpha","id":"native-cron-12"})
    );
    assert_eq!(
        c.get(ids["tracking"].as_str().unwrap())["task"]["status"],
        "waiting"
    );
    assert_eq!(ok(c.import("muse", json!([])))["imported"], 0);
    assert_eq!(
        c.get(ids["child"].as_str().unwrap())["task"]["status"],
        "pending"
    );
}

#[test]
fn bad_native_imports_are_all_or_nothing() {
    let c = Case::new();
    c.init();
    for records in [
        json!([{"id":"1","content":"valid","status":"pending"},{"id":"2","content":"invalid","status":"lost"}]),
        json!([{"id":"1","content":"first","status":"pending"},{"id":"1","content":"duplicate","status":"pending"}]),
    ] {
        assert!(!c.import("grok", records).status.success());
        assert_eq!(ok(c.run(&["list", "--all"])), json!([]));
    }
    assert!(!c.import("muse",json!([{"id":"g","kind":"goal","title":"bad","status":"pending","done_when":["missing"]}])).status.success());
    assert_eq!(ok(c.run(&["list", "--all"])), json!([]));
}

#[test]
fn cancellation_cascades_to_grandchildren_and_records_the_actor_and_reason() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "parent", None));
    let mut child = task(B);
    child["parent"] = json!(A);
    ok(c.put(&child, "child notes", None));
    let mut grandchild = task(C);
    grandchild["parent"] = json!(B);
    grandchild["status"] = json!("waiting_user");
    ok(c.put(&grandchild, "grandchild notes", None));
    ok(c.update(A, "cancelled"));
    for id in [B, C] {
        let got = c.get(id);
        assert_eq!(got["task"]["status"], "cancelled");
        assert!(c.path(id).starts_with(c.shared.join("tasks/archive")));
        let body = got["body"].as_str().unwrap();
        assert!(body.contains("| alpha |"));
        assert!(body.contains("Cancelled because"));
    }
}

#[test]
fn cancellation_reason_names_the_cancelled_ancestor_across_a_completed_child() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(A), "", None));
    let mut child = task(B);
    child["parent"] = json!(A);
    child["status"] = json!("completed");
    ok(c.put(&child, "", None));
    let mut grandchild = task(C);
    grandchild["parent"] = json!(B);
    ok(c.put(&grandchild, "", None));
    ok(c.update(A, "cancelled"));
    let got = c.get(C);
    assert_eq!(got["task"]["status"], "cancelled");
    assert!(
        got["body"]
            .as_str()
            .unwrap()
            .contains(&format!("Cancelled because ancestor {A}"))
    );
    assert_eq!(c.get(B)["task"]["status"], "completed");
}

#[test]
fn long_dependency_cycles_fail_without_changing_the_previous_task() {
    let c = Case::new();
    c.init();
    let records: Vec<_> = (0..300).map(|i| json!({"id":i.to_string(),"kind":"tracking","title":format!("work {i}"),"status":"pending","start_after":if i==0 {vec![]} else {vec![(i-1).to_string()]}})).collect();
    let imported = ok(c.import("muse", json!(records)));
    let first = imported["ids"]["0"].as_str().unwrap();
    let last = &imported["ids"]["299"];
    let old = c.get(first);
    let mut metadata = old["task"].clone();
    metadata["start_after"] = json!([last]);
    bad(
        c.put(
            &metadata,
            old["body"].as_str().unwrap(),
            old["etag"].as_str(),
        ),
        "cycle",
    );
    assert_eq!(c.get(first)["etag"], old["etag"]);
}

#[test]
fn cancelled_completion_dependencies_block_execution_through_wait_chains() {
    let c = Case::new();
    c.init();
    ok(c.put(&task(C), "prerequisite", None));
    let mut middle = task(B);
    middle["done_when"] = json!([C]);
    ok(c.put(&middle, "middle", None));
    let mut consumer = task(A);
    consumer["status"] = json!("in_progress");
    consumer["done_when"] = json!([B]);
    ok(c.put(&consumer, "consumer", None));
    assert_eq!(c.get(A)["ready"], true);
    ok(c.update(C, "cancelled"));
    assert_eq!(c.get(B)["task"]["status"], "pending");
    assert_eq!(c.get(A)["task"]["status"], "in_progress");
    for id in [A, B] {
        let got = c.get(id);
        assert_eq!(got["ready"], false);
        assert!(got["blockers"].as_array().unwrap().contains(&json!(C)));
    }
}

#[test]
fn interrupted_archive_moves_recover_without_duplicate_tasks_or_lost_history() {
    let c = Case::new();
    c.init();
    let mut active = task(A);
    active["status"] = json!("in_progress");
    ok(c.put(&active, "finished work", None));
    let mut closed = c.get(A)["task"].clone();
    closed["status"] = json!("completed");
    let raw = markdown(
        &closed,
        "finished work\n## Note\n\n## Log\n- 2026-11-11T00:00:00Z | alpha | Finished.\n",
    );
    fs::write(
        c.shared.join("tasks/.pending.json"),
        json!({"version":1,"documents":{A:raw}}).to_string(),
    )
    .unwrap();
    fs::write(c.shared.join("tasks/archive").join(format!("{A}.md")), &raw).unwrap();
    assert_eq!(ok(c.run(&["list"])), json!([]));
    assert!(!c.shared.join("tasks").join(format!("{A}.md")).exists());
    let records = ok(c.run(&["list", "--all"]));
    assert_eq!(records.as_array().unwrap().len(), 1);
    assert_eq!(records[0]["task"]["status"], "completed");
    assert!(records[0]["body"].as_str().unwrap().contains("Finished."));
}

#[test]
fn grok_import_preserves_leading_blank_lines_and_uses_a_meaningful_title() {
    let c = Case::new();
    c.init();
    let content = "\n\nPrepare rollout\nkeep all text\n";
    let result = ok(c.import(
        "grok",
        json!([{"id":"blank","content":content,"status":"pending"}]),
    ));
    let got = c.get(result["ids"]["blank"].as_str().unwrap());
    assert_eq!(got["task"]["title"], "Prepare rollout");
    assert!(got["body"].as_str().unwrap().contains(content));
}

#[test]
fn native_markdown_headings_do_not_replace_canonical_task_sections() {
    let c = Case::new();
    c.init();
    for (system, record) in [
        (
            "muse",
            json!({"id":"goal","kind":"goal","title":"outcome","status":"pending","goal":"# Outcome\n## Log\nNative log\n"}),
        ),
        (
            "grok",
            json!({"id":"goal","content":"Investigate\n## Log\nNative log\n","status":"pending"}),
        ),
    ] {
        let result = ok(c.import(system, json!([record])));
        let id = result["ids"]["goal"].as_str().unwrap();
        let old = c.get(id);
        let saved = ok(c.put(
            &old["task"],
            old["body"].as_str().unwrap(),
            old["etag"].as_str(),
        ));
        let body = saved["body"].as_str().unwrap();
        assert!(body.contains("## Log\nNative log\n"));
        assert_eq!(body.matches("## Log").count(), 2);
        assert!(body.rfind("| alpha |").unwrap() > body.rfind("## Log").unwrap());
    }
}
