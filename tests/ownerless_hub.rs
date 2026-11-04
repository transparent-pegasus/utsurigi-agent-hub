#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const KNOWLEDGE: &[u8] = "共有された知識\nkeep exact bytes\0tail\n".as_bytes();
const WAIT: Duration = Duration::from_secs(45);

// Every child has a deadline and its own process group, including real restic.
// Dropping a failed test also kills descendants and releases native locks.
struct Running(Option<Child>);

impl Running {
    fn start(mut command: Command) -> Self {
        Self(Some(
            command
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("test command must start"),
        ))
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        while self.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "child exceeded {WAIT:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
        }
    }
}

fn cli(fixture: &Fixture, bot: &Bot, args: &[&str]) -> Output {
    Running::start(fixture.cli_command(bot, args)).finish()
}

fn ok(fixture: &Fixture, bot: &Bot, args: &[&str]) -> Output {
    let output = cli(fixture, bot, args);
    // Check redaction before any diagnostic assertion could print child output.
    redacted(&output);
    success(&output);
    output
}

fn machine(bot: &mut Bot, cap: usize) {
    bot.config["machine"] = json!({
        "id": bot.config["bot"]["runtime_id"],
        "root": bot.file.parent().unwrap()
    });
    bot.config["budget"] = json!({"max_bots": cap});
    bot.config["sync"] = json!({
        "retry_lock_seconds": 1,
        "transport_retries": 0,
        "stuck_request_timeout_seconds": 5
    });
    bot.save();
}

fn initialized(cap: usize) -> Fixture {
    let mut fixture = Fixture::new();
    machine(&mut fixture.alpha, cap);
    write(fixture.alpha.path("shared").join("knowledge.md"), KNOWLEDGE);
    ok(&fixture, &fixture.alpha, &["setup"]);
    fixture
}

fn status(fixture: &Fixture, bot: &Bot) -> Value {
    serde_json::from_slice(&ok(fixture, bot, &["status"]).stdout).unwrap()
}

fn active(fixture: &Fixture, bot: &Bot) -> PathBuf {
    PathBuf::from(status(fixture, bot)["active_shared"].as_str().unwrap())
}

fn registry(root: &Path) -> Value {
    serde_saphyr::from_slice(&fs::read(root.join("registry.yaml")).unwrap()).unwrap()
}

fn roster(root: &Path) -> BTreeSet<String> {
    registry(root)["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap().to_owned())
        .collect()
}

fn head(bot: &Bot) -> Value {
    let path = Path::new(bot.repository("shared")).join("uah-head.json");
    assert!(
        path.is_file(),
        "setup must create the CAS head uah-head.json"
    );
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let to = target.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            assert!(entry.file_type().unwrap().is_file());
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

// Catches owner/member admission and registration that exports the joiner's
// stale authoring tree instead of copying the verified shared tree.
#[test]
fn unregistered_beta_joins_without_roster_edits_and_preserves_hub_knowledge() {
    let fixture = initialized(8);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 8);
    write(
        beta.path("shared").join("knowledge.md"),
        b"stale beta knowledge",
    );
    assert!(!beta.path("shared").join("registry.yaml").exists());

    ok(&fixture, &beta, &["setup"]);
    let current = active(&fixture, &beta);
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into()])
    );
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
    assert_eq!(
        fs::read(beta.path("shared").join("knowledge.md")).unwrap(),
        b"stale beta knowledge"
    );
    ok(&fixture, &fixture.alpha, &["sync"]);
    assert_eq!(roster(&active(&fixture, &fixture.alpha)), roster(&current));
    assert_eq!(
        fs::read(active(&fixture, &fixture.alpha).join("knowledge.md")).unwrap(),
        KNOWLEDGE
    );
}

// Catches initialization that leaves legacy owner/latest as the authority.
#[test]
fn new_hub_has_ownerless_v2_registry_shared_cap_and_cas_head() {
    let fixture = initialized(3);
    let current = active(&fixture, &fixture.alpha);
    let registry = registry(&current);
    assert_eq!(registry["version"], 2, "new hubs must use registry v2");
    assert!(registry.get("owner").is_none(), "v2 has no owner ACL");
    assert_eq!(registry["max_bots"], 3);
    assert_eq!(roster(&current), BTreeSet::from(["alpha".into()]));
    let head = head(&fixture.alpha);
    assert_eq!(head["version"], 2);
    assert_eq!(head["hub"], "default");
    assert!(head["revision"].as_u64().is_some());
    let snapshot = head["snapshot"].as_str().unwrap();
    assert_eq!(snapshot.len(), 64);
    assert!(snapshot.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(current.file_name().unwrap().to_str().unwrap(), snapshot);
}

// Catches blind whole-tree publication: a later publisher must not overwrite
// current knowledge using a base copied before another accepted publication.
#[test]
fn stale_authoring_base_refuses_publication_and_preserves_both_trees() {
    let fixture = initialized(8);
    let mut stale = fixture.bot("alpha", "alpha-stale");
    stale.config["bot"] = fixture.alpha.config["bot"].clone();
    machine(&mut stale, 8);
    copy_tree(&fixture.alpha.path("shared"), &stale.path("shared"));
    write(
        stale.path("shared").join("knowledge.md"),
        b"preserved stale edit",
    );

    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"accepted newer knowledge",
    );
    ok(&fixture, &fixture.alpha, &["registry", "publish"]);
    let output = cli(&fixture, &stale, &["registry", "publish"]);
    redacted(&output);
    failure(&output);
    let explanation = text(&output).to_lowercase();
    assert!(
        explanation.contains("base")
            || explanation.contains("stale")
            || explanation.contains("generation"),
        "stale publication needs an actionable explanation"
    );
    assert_eq!(
        fs::read(stale.path("shared").join("knowledge.md")).unwrap(),
        b"preserved stale edit"
    );
    ok(&fixture, &fixture.alpha, &["sync"]);
    assert_eq!(
        fs::read(active(&fixture, &fixture.alpha).join("knowledge.md")).unwrap(),
        b"accepted newer knowledge"
    );
}
