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

fn restores(tool: &Path) -> usize {
    fs::read_to_string(tool.join("calls"))
        .unwrap()
        .lines()
        .filter(|operation| *operation == "restore")
        .count()
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
fn same_snapshot_sync_succeeds_while_an_os_reader_holds_shared_flock() {
    let (fixture, mut beta) = bots();
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
    assert_eq!(restores(&tool), 0);
    release_reader(&mut reader);
}

#[test]
fn missing_accepted_snapshot_fails_closed_and_preserves_cached_versions() {
    let (fixture, mut beta) = bots();
    let first = fixture.active_shared(&beta);
    let first_id = first.file_name().unwrap().to_str().unwrap();
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
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
        restores(&tool),
        0,
        "missing accepted state must not fall back or restore a predecessor"
    );
    assert_eq!(fs::read(first.join("knowledge.md")).unwrap(), b"one");
    assert_eq!(fs::read(second.join("knowledge.md")).unwrap(), b"two");
}

#[test]
fn changed_snapshot_waits_for_reader_then_two_bots_restore_only_once() {
    let (mut fixture, mut beta) = bots();
    let first = fixture.active_shared(&beta);
    let root = first.parent().unwrap().parent().unwrap();
    let pointer = fs::read(root.join("CURRENT")).unwrap();
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let alpha_tool = tool(&mut fixture.alpha);
    let beta_tool = tool(&mut beta);
    let mut reader = reader(root);
    let mut alpha_sync = Running::start(fixture.cli_command(&fixture.alpha, &["sync"]));
    let mut beta_sync = Running::start(fixture.cli_command(&beta, &["sync"]));
    alpha_sync.reached(&alpha_tool.join("snapshots-returned"));
    beta_sync.reached(&beta_tool.join("snapshots-returned"));
    alpha_sync.stays_running();
    beta_sync.stays_running();
    assert_eq!(restores(&alpha_tool) + restores(&beta_tool), 0);
    assert_no_writes(&alpha_tool);
    assert_no_writes(&beta_tool);
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), pointer);
    release_reader(&mut reader);
    success(&alpha_sync.finish());
    success(&beta_sync.finish());
    assert_eq!(restores(&alpha_tool) + restores(&beta_tool), 1);
    let second = fixture.active_shared(&beta);
    assert_ne!(second, first);
    assert_eq!(fixture.active_shared(&fixture.alpha), second);
    assert_eq!(fs::read(second.join("knowledge.md")).unwrap(), b"two");
    assert!(first.is_dir());
}

#[test]
fn stalled_upload_releases_cache_and_late_completion_cannot_revert_newer_current() {
    let (mut fixture, mut beta) = bots();
    let first = fixture.active_shared(&beta);
    let root = first.parent().unwrap().parent().unwrap();
    // A second command configuration for the current runtime can publish while
    // its data upload holds the original per-Bot state lock.
    let mut publisher = Bot {
        file: fixture.temp.path().join("publisher.yaml"),
        config: fixture.alpha.config.clone(),
    };
    publisher.config["paths"]["state"] = json!(fixture.temp.path().join("publisher-state"));
    publisher.save();
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    fixture.ok(&publisher, &["registry", "publish"]);
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
    assert_eq!(restores(&alpha_tool), 1);
    assert_eq!(
        restores(&beta_tool),
        0,
        "reuse another Bot's complete unpublished restore"
    );

    write(fixture.alpha.path("shared").join("knowledge.md"), b"three");
    fixture.ok(&publisher, &["registry", "publish"]);
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
    release_reader(&mut reader);
}

#[test]
fn failed_upload_preserves_current_and_retry_reuses_complete_unpublished_restore() {
    let (fixture, mut beta) = bots();
    let first = fixture.active_shared(&beta);
    let before = fixture.status(&beta);
    write(fixture.alpha.path("shared").join("knowledge.md"), b"two");
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    let tool = tool(&mut beta);
    fs::write(tool.join("mode"), "fail-backup").unwrap();
    failure(&fixture.cli(&beta, &["sync"]));
    assert_eq!(fixture.active_shared(&beta), first);
    assert_eq!(fixture.status(&beta), before);
    fs::write(tool.join("mode"), "pass").unwrap();
    fixture.ok(&beta, &["sync"]);
    assert_ne!(fixture.active_shared(&beta), first);
    assert_eq!(restores(&tool), 1);
}
