#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

// Kill the whole process group on failure, including a stalled restic child.
struct Running(Option<Child>);

impl Running {
    fn start(mut command: Command) -> Self {
        Self(Some(
            command
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ))
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "child operation did not finish within 20 seconds"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }

    fn reached(&mut self, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !path.exists() {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                panic!(
                    "child exited before {}: {}",
                    path.display(),
                    text(&self.finish())
                );
            }
            assert!(
                Instant::now() < deadline,
                "child never reached {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn stays_running(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                panic!("operation should wait for reader: {}", text(&self.finish()));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

fn bots() -> (Fixture, Bot) {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["paths"]["imports"] = fixture.alpha.config["paths"]["imports"].clone();
    beta.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    write(fixture.alpha.path("shared").join("knowledge.md"), b"one");
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    (fixture, beta)
}

fn tool(bot: &mut Bot) -> PathBuf {
    let tool = Fixture::wrapper(bot);
    let script = include_str!("fixtures/restic-wrapper.sh").replace(
        "exec \"$fixture_dir/real-restic\" \"$@\"\n",
        r#"case "$mode:$operation" in
    stall-backup:backup)
        : > "$fixture_dir/backup-started"
        while [ ! -e "$fixture_dir/release" ]; do sleep 0.01; done
        ;;
    fail-backup:backup) printf '%s\n' 'synthetic upload failure' >&2; exit 1 ;;
esac
if [ "$operation" = snapshots ]; then
    "$fixture_dir/real-restic" "$@"
    : > "$fixture_dir/snapshots-returned"
else
    exec "$fixture_dir/real-restic" "$@"
fi
"#,
    );
    fs::write(tool.join("restic"), script).unwrap();
    tool
}

fn publisher(fixture: &Fixture) -> Bot {
    let mut publisher = Bot {
        file: fixture.temp.path().join("publisher.yaml"),
        config: fixture.alpha.config.clone(),
    };
    publisher.config["paths"]["imports"] = json!(fixture.temp.path().join("publisher-imports"));
    publisher.config["paths"]["state"] = json!(fixture.temp.path().join("publisher-state"));
    publisher.save();
    publisher
}

fn publish_remote(fixture: &Fixture, publisher: &Bot) -> String {
    let current = fixture.active_shared(&fixture.alpha);
    let pointer = current.parent().unwrap().parent().unwrap().join("CURRENT");
    let before = fs::read(&pointer).unwrap();
    fixture.ok(publisher, &["registry", "publish"]);
    let accepted = fixture.current_shared_id(publisher);
    assert_eq!(
        fixture
            .active_shared(publisher)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
        accepted
    );
    assert_eq!(fixture.active_shared(&fixture.alpha), current);
    assert_eq!(
        fs::read(pointer).unwrap(),
        before,
        "remote publication must leave the reader machine's pointer pending"
    );
    accepted
}

fn reader(root: &Path) -> Running {
    let ready = root.join("reader-ready");
    let mut command = Command::new("flock");
    command
        .arg("--shared")
        .arg(root.join("operation.lock"))
        .args(["sh", "-c", ": > \"$1\"; cat >/dev/null", "reader"])
        .arg(&ready);
    let mut process = Running::start(command);
    process.reached(&ready);
    process
}

fn release_reader(reader: &mut Running) {
    drop(reader.0.as_mut().unwrap().stdin.take());
    success(&reader.finish());
}

#[test]
fn owned_backup_keeps_captured_bytes_stable_while_the_live_source_changes() {
    let (fixture, mut beta) = bots();
    let source = beta.path("owned").join("notes/changing.md");
    write(&source, b"before");
    let tool = tool(&mut beta);
    fs::write(tool.join("mode"), "stall-backup").unwrap();
    let mut sync = Running::start(fixture.cli_command(&beta, &["sync"]));
    sync.reached(&tool.join("backup-started"));
    fs::write(&source, b"after!").unwrap();
    fs::write(tool.join("release"), "").unwrap();
    success(&sync.finish());
    let snapshots = fixture.ids(&beta, "bot");
    assert_eq!(snapshots.len(), 1);
    let destination = fixture.temp.path().join("stable-capture");
    fixture.ok(
        &beta,
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
        fs::read(destination.join("notes/changing.md")).unwrap(),
        b"before"
    );
    assert_eq!(fs::read(source).unwrap(), b"after!");
}

#[test]
fn same_snapshot_sync_succeeds_while_an_os_reader_holds_shared_flock() {
    let (fixture, mut beta) = bots();
    let control = shared_cache_control(&fixture, &beta);
    let tool = tool(&mut beta);
    let first = fixture.active_shared(&beta);
    let root = first.parent().unwrap().parent().unwrap();
    let pointer = fs::read(root.join("CURRENT")).unwrap();
    let accepted = fixture.shared_head(&beta);
    assert_eq!(
        accepted["snapshot"],
        first.file_name().unwrap().to_str().unwrap()
    );
    let mut reader = reader(root);
    let mut sync = Running::start(fixture.cli_command(&beta, &["sync"]));
    success(&sync.finish());
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), pointer);
    assert_eq!(fixture.active_shared(&beta), first);
    assert_eq!(fixture.shared_head(&beta), accepted);
    assert!(shared_data_restores(&beta, &tool, &control, &[]).is_empty());
    release_reader(&mut reader);
}

#[test]
fn missing_accepted_snapshot_fails_closed_and_preserves_cached_versions() {
    let (fixture, mut beta) = bots();
    let publisher = publisher(&fixture);
    let control = shared_cache_control(&fixture, &beta);
    let first = fixture.active_shared(&beta);
    let first_id = first.file_name().unwrap().to_str().unwrap();
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    publish_remote(&fixture, &publisher);
    fixture.ok(&beta, &["sync"]);
    let second = fixture.active_shared(&beta);
    let second_id = second.file_name().unwrap().to_str().unwrap();
    assert_ne!(first, second);
    let root = second.parent().unwrap().parent().unwrap();
    let pointer_bytes = fs::read(root.join("CURRENT")).unwrap();
    let pointer: Value = serde_json::from_slice(&pointer_bytes).unwrap();
    assert_eq!(pointer["current"], second_id);
    assert_eq!(pointer["previous"], first_id);

    let accepted = fixture.shared_head(&beta);
    assert_eq!(accepted["snapshot"], second_id);
    let before = fixture.status(&beta);
    let owned = fixture.ids(&beta, "bot");
    let mut remaining = fixture.ids(&fixture.alpha, "shared");
    assert!(remaining.remove(second_id));
    // External loss of the accepted state must never promote an older cached
    // snapshot. Its immutable marker cannot carry a newer head revision.
    success(&fixture.restic(&fixture.alpha, "shared", &["forget", second_id]));
    assert_eq!(fixture.ids(&fixture.alpha, "shared"), remaining);
    if root.join("pending").exists() {
        fs::remove_dir(root.join("pending")).unwrap();
    }
    let tool = tool(&mut beta);
    failure(&fixture.cli(&beta, &["sync"]));
    assert_eq!(fixture.status(&beta), before);
    assert_eq!(fixture.active_shared(&beta), second);
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), pointer_bytes);
    assert_eq!(fixture.shared_head(&beta), accepted);
    assert_eq!(fixture.ids(&beta, "bot"), owned);
    assert_no_writes(&tool);
    assert_eq!(
        shared_data_restores(&beta, &tool, &control, &[]),
        Vec::<String>::new(),
        "missing accepted state must not fall back or restore a predecessor"
    );
    assert_eq!(fs::read(first.join("knowledge.md")).unwrap(), b"one");
    assert_eq!(fs::read(second.join("knowledge.md")).unwrap(), b"two");
}

#[test]
fn changed_snapshot_waits_for_reader_then_two_bots_restore_only_once() {
    let (mut fixture, mut beta) = bots();
    let publisher = publisher(&fixture);
    let control = shared_cache_control(&fixture, &beta);
    let first = fixture.active_shared(&beta);
    let root = first.parent().unwrap().parent().unwrap();
    let pointer = fs::read(root.join("CURRENT")).unwrap();
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    let accepted = publish_remote(&fixture, &publisher);
    let alpha_tool = tool(&mut fixture.alpha);
    let beta_tool = tool(&mut beta);
    let mut reader = reader(root);
    let mut alpha_sync = Running::start(fixture.cli_command(&fixture.alpha, &["sync"]));
    let mut beta_sync = Running::start(fixture.cli_command(&beta, &["sync"]));
    alpha_sync.reached(&alpha_tool.join("snapshots-returned"));
    beta_sync.reached(&beta_tool.join("snapshots-returned"));
    alpha_sync.stays_running();
    beta_sync.stays_running();
    assert!(shared_data_restores(&fixture.alpha, &alpha_tool, &control, &[]).is_empty());
    assert!(shared_data_restores(&beta, &beta_tool, &control, &[]).is_empty());
    assert_no_writes(&alpha_tool);
    assert_no_writes(&beta_tool);
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), pointer);
    release_reader(&mut reader);
    success(&alpha_sync.finish());
    success(&beta_sync.finish());
    let restored = [
        shared_data_restores(&fixture.alpha, &alpha_tool, &control, &[&accepted]),
        shared_data_restores(&beta, &beta_tool, &control, &[&accepted]),
    ]
    .concat();
    assert_eq!(restored, vec![accepted.clone()]);
    let second = fixture.active_shared(&beta);
    assert_ne!(second, first);
    assert_eq!(second.file_name().unwrap().to_str().unwrap(), accepted);
    assert_eq!(fixture.active_shared(&fixture.alpha), second);
    assert_eq!(fs::read(second.join("knowledge.md")).unwrap(), b"two");
    assert!(first.is_dir());
}

#[test]
fn stalled_upload_releases_cache_and_late_completion_cannot_revert_newer_current() {
    let (mut fixture, mut beta) = bots();
    let first = fixture.active_shared(&beta);
    let root = first.parent().unwrap().parent().unwrap();
    let publisher = publisher(&fixture);
    let control = shared_cache_control(&fixture, &beta);
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    let second_id = publish_remote(&fixture, &publisher);
    let alpha_tool = tool(&mut fixture.alpha);
    let beta_tool = tool(&mut beta);
    fs::write(alpha_tool.join("mode"), "stall-backup").unwrap();
    let mut alpha_sync = Running::start(fixture.cli_command(&fixture.alpha, &["sync"]));
    alpha_sync.reached(&alpha_tool.join("backup-started"));
    assert_eq!(
        fixture.active_shared(&beta),
        first,
        "an unfinished upload must not switch CURRENT"
    );
    let busy = fixture.cli(&fixture.alpha, &["sync"]);
    failure(&busy);
    assert!(text(&busy).contains("Bot state"), "{}", text(&busy));
    assert!(
        text(&busy).contains(
            fixture
                .alpha
                .path("state")
                .join("operation.lock")
                .to_str()
                .unwrap()
        )
    );
    let mut beta_sync = Running::start(fixture.cli_command(&beta, &["sync"]));
    success(&beta_sync.finish());
    let second = fixture.active_shared(&beta);
    assert_ne!(second, first);
    assert_eq!(
        shared_data_restores(&fixture.alpha, &alpha_tool, &control, &[&second_id]),
        vec![second_id.clone()]
    );
    assert_eq!(
        shared_data_restores(&beta, &beta_tool, &control, &[]),
        Vec::<String>::new(),
        "reuse another Bot's complete unpublished restore"
    );

    write(fixture.alpha.path("shared").join("knowledge.md"), b"three");
    let third_id = publish_remote(&fixture, &publisher);
    fixture.ok(&beta, &["sync"]);
    let third = fixture.active_shared(&beta);
    assert_ne!(third, second);
    let pointer = fs::read(root.join("CURRENT")).unwrap();
    let mut reader = reader(root);
    fs::write(alpha_tool.join("release"), "").unwrap();
    success(&alpha_sync.finish());
    assert_eq!(
        fs::read(root.join("CURRENT")).unwrap(),
        pointer,
        "late upload must not overwrite newer pointer"
    );
    assert_eq!(fs::read(third.join("knowledge.md")).unwrap(), b"three");
    assert!(second.is_dir());
    assert!(!first.exists(), "retain only current and previous versions");
    let pointer: Value = serde_json::from_slice(&pointer).unwrap();
    assert_eq!(
        pointer["previous"],
        second.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(
        shared_data_restores(&fixture.alpha, &alpha_tool, &control, &[&second_id]),
        vec![second_id]
    );
    assert_eq!(
        shared_data_restores(&beta, &beta_tool, &control, &[&third_id]),
        vec![third_id]
    );
    release_reader(&mut reader);
}

#[test]
fn failed_upload_preserves_current_and_retry_reuses_complete_unpublished_restore() {
    let (fixture, mut beta) = bots();
    let publisher = publisher(&fixture);
    let control = shared_cache_control(&fixture, &beta);
    let first = fixture.active_shared(&beta);
    let before = fixture.status(&beta);
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    let accepted = publish_remote(&fixture, &publisher);
    let tool = tool(&mut beta);
    fs::write(tool.join("mode"), "fail-backup").unwrap();
    failure(&fixture.cli(&beta, &["sync"]));
    assert_eq!(fixture.active_shared(&beta), first);
    assert_eq!(fixture.status(&beta), before);
    fs::write(tool.join("mode"), "pass").unwrap();
    assert!(fixture.ids(&beta, "bot").is_empty());
    fs::remove_file(beta.path("state").join("pending-backup.yaml")).unwrap();
    fixture.ok(&beta, &["sync"]);
    assert_ne!(fixture.active_shared(&beta), first);
    assert_eq!(
        shared_data_restores(&beta, &tool, &control, &[&accepted]),
        vec![accepted]
    );
}

fn tree_bytes(root: &Path) -> u64 {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                tree_bytes(&entry.path())
            } else {
                assert!(kind.is_file(), "cache must contain regular data");
                entry.metadata().unwrap().len()
            }
        })
        .sum()
}

fn bounded_pending_after(operation: &str) {
    let (fixture, mut beta) = bots();
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.author_current(&beta);
    let publisher = publisher(&fixture);
    let tool = tool(&mut beta);
    let initial = fixture.active_shared(&beta);
    let root = initial.parent().unwrap().parent().unwrap();
    let mut validated = std::collections::BTreeMap::new();
    for entry in fs::read_dir(root.join("versions")).unwrap() {
        let path = entry.unwrap().path();
        validated.insert(
            path.file_name().unwrap().to_str().unwrap().to_owned(),
            fs::read(path.join("knowledge.md")).unwrap(),
        );
    }
    let mut usage = Vec::new();
    let mut largest_tree = 0;
    for generation in 1..=3 {
        let knowledge = vec![b'1' + generation; 64 * 1024];
        write(
            fixture.alpha.path("shared").join("knowledge.md"),
            &knowledge,
        );
        let accepted = publish_remote(&fixture, &publisher);
        let expected_head = fixture.shared_head(&publisher);
        validated.insert(accepted, knowledge);
        largest_tree = largest_tree.max(tree_bytes(&fixture.active_shared(&publisher)));
        let before = fs::read(root.join("CURRENT")).unwrap();
        fs::write(tool.join("calls"), "").unwrap();
        match operation {
            "pull" => {
                let destination = fixture.temp.path().join(format!("pull-{generation}"));
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
                    fs::read(destination.join("notes/日本語.md")).unwrap(),
                    PAYLOAD
                );
            }
            "prune" => {
                fixture.ok(&beta, &["prune"]);
            }
            "rejected-publish" => {
                failure(&fixture.cli(&beta, &["registry", "publish"]));
                assert_no_writes(&tool);
                assert_eq!(fs::read(root.join("CURRENT")).unwrap(), before);
            }
            "failed-sync" => {
                fs::write(tool.join("mode"), "fail-backup").unwrap();
                let output = fixture.cli(&beta, &["sync"]);
                failure(&output);
                assert!(
                    text(&output).contains("synthetic upload failure"),
                    "{}",
                    text(&output)
                );
                assert_eq!(fs::read(root.join("CURRENT")).unwrap(), before);
                assert!(fixture.ids(&beta, "bot").is_empty());
                fs::write(tool.join("mode"), "pass").unwrap();
                // Reconcile the deliberately failed pre-upload attempt, as in the retry race above.
                fs::remove_file(beta.path("state").join("pending-backup.yaml")).unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            fixture.shared_head(&beta),
            expected_head,
            "{operation} must not publish a rejected or failed candidate"
        );
        let current = fixture.active_shared(&beta);
        let pointer: Value =
            serde_json::from_slice(&fs::read(root.join("CURRENT")).unwrap()).unwrap();
        assert_eq!(
            pointer["current"],
            current.file_name().unwrap().to_str().unwrap()
        );
        for id in [pointer["current"].as_str(), pointer["previous"].as_str()]
            .into_iter()
            .flatten()
        {
            assert_eq!(
                fs::read(root.join("versions").join(id).join("knowledge.md")).unwrap(),
                *validated
                    .get(id)
                    .expect("CURRENT must only reference validated generations")
            );
        }
        assert!(
            fs::read_dir(root.join("versions")).unwrap().count() <= 2,
            "retain only CURRENT current/previous versions"
        );
        assert_eq!(
            fs::read(beta.path("owned").join("notes/日本語.md")).unwrap(),
            PAYLOAD
        );
        let pending = root.join("pending");
        let (count, bytes) = if pending.exists() {
            (
                fs::read_dir(&pending).unwrap().count(),
                tree_bytes(&pending),
            )
        } else {
            (0, 0)
        };
        eprintln!(
            "{operation} generation {generation}: pending generations={count}, bytes={bytes}"
        );
        usage.push((count, bytes));
    }
    assert!(
        usage
            .iter()
            .all(|(count, bytes)| *count <= 1 && *bytes <= largest_tree),
        "{operation} accumulated pending cache storage: {usage:?}; retain at most one sequential pending generation ({largest_tree} bytes)"
    );
}

#[test]
fn successive_pull_only_imports_keep_pending_cache_storage_bounded() {
    bounded_pending_after("pull");
}

#[test]
fn successive_prunes_keep_pending_cache_storage_bounded() {
    bounded_pending_after("prune");
}

#[test]
fn successive_rejected_publications_keep_pending_cache_storage_bounded() {
    bounded_pending_after("rejected-publish");
}

#[test]
fn successive_failed_syncs_keep_pending_cache_storage_bounded() {
    bounded_pending_after("failed-sync");
}
