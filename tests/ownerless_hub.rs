#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const KNOWLEDGE: &[u8] = "共有された知識\nkeep exact bytes\0tail\n".as_bytes();
const WAIT: Duration = Duration::from_secs(45);

// Every child has a deadline and its own process group, including real restic.
// Dropping a failed test also kills descendants and releases native locks.
struct Running {
    child: Option<Child>,
    stdout: Receiver<Vec<u8>>,
    stderr: Receiver<Vec<u8>>,
}

fn drain(mut stream: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap();
        let _ = sender.send(bytes);
    });
    receiver
}

impl Running {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("test command must start");
        Self {
            stdout: drain(child.stdout.take().unwrap()),
            stderr: drain(child.stderr.take().unwrap()),
            child: Some(child),
        }
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "child exceeded {WAIT:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        // Drain concurrently and bound pipe closure too: an orphaned descendant
        // must not leave wait_with_output hanging after its parent has exited.
        let stdout = self
            .stdout
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("stdout pipe did not close before deadline");
        let stderr = self
            .stderr
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("stderr pipe did not close before deadline");
        self.child.take();
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn reached(&mut self, path: &Path) {
        let deadline = Instant::now() + WAIT;
        while !path.exists() {
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                let output = self.finish();
                redacted(&output);
                panic!(
                    "child exited before barrier {}: {}",
                    path.display(),
                    text(&output)
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
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
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
    let path = bot.path("shared").join("registry.yaml");
    if path.is_file() {
        let mut value = registry(&bot.path("shared"));
        if value["version"] == 2 {
            value["max_bots"] = json!(cap);
            fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        }
    }
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

fn restic_tool(bot: &Bot) -> PathBuf {
    let configured = PathBuf::from(bot.config["encryption"]["executable"].as_str().unwrap());
    let real = configured.parent().unwrap().join("real-restic");
    if real.exists() { real } else { configured }
}

fn restic_command(
    fixture: &Fixture,
    bot: &Bot,
    repository: &str,
    cwd: &Path,
    args: &[&str],
) -> Command {
    let password = if repository == "shared" {
        SHARED_PASSWORD
    } else if bot.config["bot"]["id"] == "alpha" {
        BOT_PASSWORD
    } else {
        PEER_PASSWORD
    };
    let mut command = Command::new(restic_tool(bot));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("RESTIC_PASSWORD", password)
        .env("RESTIC_CACHE_DIR", fixture.temp.path().join("raw-cache"))
        .env("TMPDIR", fixture.temp.path())
        .current_dir(cwd)
        .args(["--repo", bot.repository(repository), "--retry-lock", "1s"])
        .args(args);
    command
}

fn raw(fixture: &Fixture, bot: &Bot, repository: &str, cwd: &Path, args: &[&str]) -> Output {
    let output = Running::start(restic_command(fixture, bot, repository, cwd, args)).finish();
    redacted(&output);
    success(&output);
    output
}

fn snapshots(fixture: &Fixture, bot: &Bot, repository: &str) -> Vec<Value> {
    serde_json::from_slice(
        &raw(
            fixture,
            bot,
            repository,
            &bot.path("shared"),
            &["snapshots", "--json"],
        )
        .stdout,
    )
    .unwrap()
}

fn snapshot_ids(fixture: &Fixture, bot: &Bot) -> BTreeSet<String> {
    snapshots(fixture, bot, "shared")
        .iter()
        .map(|value| value["id"].as_str().unwrap().to_owned())
        .collect()
}

fn wrapper(bot: &mut Bot, mode: &str) -> PathBuf {
    let real = restic_tool(bot);
    let directory = bot.file.parent().unwrap().join("ownerless-restic");
    fs::create_dir_all(&directory).unwrap();
    symlink(real, directory.join("real-restic")).unwrap();
    let tool = directory.join("restic");
    fs::write(&tool, include_str!("fixtures/ownerless-restic.sh")).unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(directory.join("mode"), mode).unwrap();
    fs::write(directory.join("calls"), "").unwrap();
    bot.config["encryption"]["executable"] = json!(tool);
    bot.save();
    directory
}

fn author_current(fixture: &Fixture, bot: &Bot) {
    let source = active(fixture, bot);
    fs::remove_dir_all(bot.path("shared")).unwrap();
    copy_tree(&source, &bot.path("shared"));
    // Existing reserved generated history is regenerated by publication.
    fs::remove_file(bot.path("shared").join("registry-history.json")).unwrap();
}

fn backup_tree(
    fixture: &Fixture,
    bot: &Bot,
    repository: &str,
    source: &Path,
    (bot_id, runtime, hub): (&str, &str, &str),
    completed: i64,
) -> String {
    let kind = if repository == "shared" {
        "shared"
    } else {
        "bot"
    };
    let output = raw(
        fixture,
        bot,
        repository,
        source,
        &[
            "backup",
            ".",
            "--json",
            "--host",
            bot_id,
            "--tag",
            "utsurigi-agent-hub",
            "--tag",
            &format!("hub:{hub}"),
            "--tag",
            &format!("bot:{bot_id}"),
            "--tag",
            &format!("runtime:{runtime}"),
            "--tag",
            &format!("kind:{kind}"),
            "--tag",
            &format!("completed:{completed}"),
        ],
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|value| value["snapshot_id"].as_str().map(str::to_owned))
        .expect("backup summary must identify a snapshot")
}

// Build genuine v1 storage without depending on any supplied old executable.
// Extra retired identifiers make preservation of cumulative history observable.
fn legacy(fixture: &Fixture, bots: &[&Bot]) -> (String, Value) {
    fixture.legacy_registry(&fixture.alpha, bots);
    write(fixture.alpha.path("shared").join("knowledge.md"), KNOWLEDGE);
    raw(
        fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["init"],
    );
    for bot in bots {
        raw(fixture, bot, "bot", &bot.path("shared"), &["init"]);
    }
    let repository_id = |bot: &Bot, name| -> String {
        let config: Value = serde_json::from_slice(
            &raw(fixture, bot, name, &bot.path("shared"), &["cat", "config"]).stdout,
        )
        .unwrap();
        config["id"].as_str().unwrap().to_owned()
    };
    let shared_bots: Vec<_> = bots
        .iter()
        .filter(|bot| bot.config["sharing"]["bot_data"] == true)
        .collect();
    let mut urls = vec![
        fixture.alpha.repository("shared").to_owned(),
        fixture
            .temp
            .path()
            .join("retired-repository")
            .display()
            .to_string(),
    ];
    urls.extend(
        shared_bots
            .iter()
            .map(|bot| bot.repository("bot").to_owned()),
    );
    let mut envs = vec![
        "TEST_HUB_SHARED_PASSWORD".to_owned(),
        "RETIRED_HUB_PASSWORD".to_owned(),
    ];
    envs.extend(shared_bots.iter().map(|bot| {
        bot.config["repositories"]["bot"]["password_env"]
            .as_str()
            .unwrap()
            .to_owned()
    }));
    let mut refs = vec![
        "op://synthetic/shared/password".to_owned(),
        "op://synthetic/retired/password".to_owned(),
    ];
    refs.extend(shared_bots.iter().map(|bot| {
        bot.config["repositories"]["bot"]["password_ref"]
            .as_str()
            .unwrap()
            .to_owned()
    }));
    let mut ids = vec![repository_id(&fixture.alpha, "shared"), "c".repeat(64)];
    ids.extend(shared_bots.iter().map(|bot| repository_id(bot, "bot")));
    let history = json!({"version":1, "repository_urls":urls, "password_envs":envs, "password_refs":refs, "repository_ids":ids});
    write(
        fixture.alpha.path("shared").join("registry-history.json"),
        serde_json::to_vec_pretty(&history).unwrap(),
    );
    let id = backup_tree(
        fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        ("alpha", "alpha-1", "default"),
        1_700_000_000,
    );
    (id, history)
}

fn assert_history_preserved(root: &Path, before: &Value) {
    let after: Value =
        serde_json::from_slice(&fs::read(root.join("registry-history.json")).unwrap()).unwrap();
    for field in [
        "repository_urls",
        "password_envs",
        "password_refs",
        "repository_ids",
    ] {
        let after = after[field].as_array().unwrap();
        for value in before[field].as_array().unwrap() {
            assert!(
                after.contains(value),
                "migration lost an identifier in {field}"
            );
        }
    }
}

fn assert_unfenced(fixture: &Fixture, original: &str) {
    assert!(
        !Path::new(fixture.alpha.repository("shared"))
            .join("uah-head.json")
            .exists(),
        "failed v1 validation must not publish a v2 head"
    );
    let all = snapshots(fixture, &fixture.alpha, "shared");
    assert!(all.iter().any(|snapshot| snapshot["id"] == original));
    for snapshot in all {
        let tags: Vec<_> = snapshot["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            tags.iter()
                .filter(|tag| tag.starts_with("hub:"))
                .copied()
                .collect::<Vec<_>>(),
            ["hub:default"],
            "failed validation must leave every snapshot acceptable to the v1 namespace check"
        );
        assert!(
            !tags.iter().any(|tag| tag.starts_with("uah2:")),
            "failed validation must not create an incompatible fence"
        );
    }
}

fn decision_yaml(bot: &Bot, enabled: bool) -> String {
    include_str!("fixtures/ownerless-decisions.yaml")
        .replace("__HUB__", bot.config["hub"].as_str().unwrap())
        .replace(
            "__REPOSITORY_SHA256__",
            &format!("{:x}", Sha256::digest(bot.repository("shared").as_bytes())),
        )
        .replace(
            "enabled: true",
            if enabled {
                "enabled: true"
            } else {
                "enabled: false"
            },
        )
}

fn assert_no_invented_consent(root: &Path) {
    let path = root.join("decisions.yaml");
    if path.exists() {
        let decision: Value = serde_saphyr::from_slice(&fs::read(path).unwrap()).unwrap();
        if let Some(record) = decision.get("caveman").filter(|record| !record.is_null()) {
            assert_eq!(
                record["basis"], "derived_default",
                "an ordinary template default must never invent user consent"
            );
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
    assert_eq!(head["revision"], 1);
    let all = snapshots(&fixture, &fixture.alpha, "shared");
    let accepted = all.iter().find(|value| value["id"] == snapshot).unwrap();
    assert!(
        accepted["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("kind:shared-v2"))
    );
    let fence = all
        .iter()
        .find(|snapshot| {
            snapshot["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("uah2:kind:format-fence"))
        })
        .expect("new hubs must fence legacy initialization before creating their first head");
    let tags = fence["tags"].as_array().unwrap();
    assert!(tags.contains(&json!("uah2:format:2")));
    assert!(tags.contains(&json!("uah2:hub:default")));
    assert!(
        !tags
            .iter()
            .filter_map(Value::as_str)
            .any(|tag| tag.starts_with("hub:"))
    );
    let restored = fixture.temp.path().join("empty-fence");
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &[
            "restore",
            fence["id"].as_str().unwrap(),
            "--target",
            restored.to_str().unwrap(),
            "--verify",
        ],
    );
    let payload: Value =
        serde_json::from_slice(&fs::read(restored.join("format-fence.json")).unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"type":"utsurigi-agent-hub/format-fence", "format":2, "hub":"default", "source":null, "capture":null})
    );
    for snapshot in all {
        assert!(
            !snapshot["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("kind:shared")),
            "v2 states must never look like legacy latest-shared snapshots"
        );
    }
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

// Both candidates reach immutable-state backup before either can publish.
// The loser must re-read the common head and retry its roster-only change.
#[test]
fn concurrent_distinct_joins_retain_every_row_and_exact_knowledge() {
    let fixture = initialized(8);
    let mut beta = fixture.bot("beta", "beta-1");
    let mut gamma = fixture.bot("gamma", "gamma-1");
    machine(&mut beta, 8);
    machine(&mut gamma, 8);
    let beta_tool = wrapper(&mut beta, "barrier-backup");
    let gamma_tool = wrapper(&mut gamma, "barrier-backup");
    let mut beta_join = Running::start(fixture.cli_command(&beta, &["setup"]));
    let mut gamma_join = Running::start(fixture.cli_command(&gamma, &["setup"]));
    beta_join.reached(&beta_tool.join("ready"));
    gamma_join.reached(&gamma_tool.join("ready"));
    write(beta_tool.join("release"), b"");
    write(gamma_tool.join("release"), b"");
    let beta_result = beta_join.finish();
    let gamma_result = gamma_join.finish();
    redacted(&beta_result);
    redacted(&gamma_result);
    success(&beta_result);
    success(&gamma_result);
    ok(&fixture, &fixture.alpha, &["sync"]);
    let current = active(&fixture, &fixture.alpha);
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into(), "gamma".into()])
    );
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
}

#[test]
fn competing_for_last_slot_obeys_shared_cap_despite_different_local_caps() {
    let fixture = initialized(2);
    let mut beta = fixture.bot("beta", "beta-1");
    let mut gamma = fixture.bot("gamma", "gamma-1");
    machine(&mut beta, 1);
    machine(&mut gamma, 99);
    let beta_tool = wrapper(&mut beta, "barrier-backup");
    let gamma_tool = wrapper(&mut gamma, "barrier-backup");
    let mut beta_join = Running::start(fixture.cli_command(&beta, &["setup"]));
    let mut gamma_join = Running::start(fixture.cli_command(&gamma, &["setup"]));
    beta_join.reached(&beta_tool.join("ready"));
    gamma_join.reached(&gamma_tool.join("ready"));
    write(beta_tool.join("release"), b"");
    write(gamma_tool.join("release"), b"");
    let results = [beta_join.finish(), gamma_join.finish()];
    for result in &results {
        redacted(result);
    }
    assert_eq!(
        results
            .iter()
            .filter(|result| result.status.success())
            .count(),
        1,
        "exactly one registration may fill the shared final slot"
    );
    let loser = results
        .iter()
        .find(|result| !result.status.success())
        .unwrap();
    let reason = text(loser).to_lowercase();
    assert!(
        reason.contains("cap")
            || reason.contains("max_bots")
            || reason.contains("limit")
            || reason.contains("fleet"),
        "loser must explain the shared fleet limit"
    );
    ok(&fixture, &fixture.alpha, &["sync"]);
    let current = active(&fixture, &fixture.alpha);
    assert_eq!(registry(&current)["max_bots"], 2);
    assert_eq!(roster(&current).len(), 2);
    assert!(roster(&current).contains("alpha"));
    // Inspect every published candidate, not just the eventual winner: no
    // rejected contender may even author a state larger than the shared cap.
    for snapshot in snapshots(&fixture, &fixture.alpha, "shared") {
        let id = snapshot["id"].as_str().unwrap();
        let tree = fixture.temp.path().join(format!("candidate-{id}"));
        raw(
            &fixture,
            &fixture.alpha,
            "shared",
            &fixture.alpha.path("shared"),
            &[
                "restore",
                id,
                "--target",
                tree.to_str().unwrap(),
                "--verify",
            ],
        );
        if tree.join("registry.yaml").is_file() {
            let value = registry(&tree);
            if value["version"] == 2 {
                assert!(value["bots"].as_array().unwrap().len() <= 2);
            }
        }
    }
}

#[test]
fn established_shared_cap_overrides_smaller_joiner_local_cap_and_retry_is_idempotent() {
    let fixture = initialized(8);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 1);
    ok(&fixture, &beta, &["setup"]);
    let first = registry(&active(&fixture, &beta));
    let before = head(&beta);
    ok(&fixture, &beta, &["setup"]);
    assert_eq!(registry(&active(&fixture, &beta)), first);
    assert_eq!(roster(&active(&fixture, &beta)).len(), 2);
    assert_eq!(first["max_bots"], 8);
    assert!(head(&beta)["revision"].as_u64().unwrap() >= before["revision"].as_u64().unwrap());
    assert_eq!(
        fs::read(active(&fixture, &beta).join("knowledge.md")).unwrap(),
        KNOWLEDGE
    );
}

#[test]
fn conflicting_same_id_runtime_refuses_without_losing_local_work_or_current_head() {
    let fixture = initialized(8);
    let mut clone = fixture.bot("alpha", "alpha-unconfirmed");
    machine(&mut clone, 8);
    write(
        clone.path("shared").join("knowledge.md"),
        b"unconfirmed local edit",
    );
    let before = head(&fixture.alpha);
    let output = cli(&fixture, &clone, &["setup"]);
    redacted(&output);
    failure(&output);
    assert_eq!(head(&fixture.alpha), before);
    assert_eq!(
        fs::read(clone.path("owned").join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
    assert_eq!(
        fs::read(clone.path("shared").join("knowledge.md")).unwrap(),
        b"unconfirmed local edit"
    );
}

#[test]
fn joining_never_blesses_old_local_authoring_tree_for_publication() {
    let fixture = initialized(8);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 8);
    write(
        beta.path("shared").join("knowledge.md"),
        b"unbased beta edit",
    );
    ok(&fixture, &beta, &["setup"]);
    let current = active(&fixture, &beta);
    fs::copy(
        current.join("registry.yaml"),
        beta.path("shared").join("registry.yaml"),
    )
    .unwrap();
    let before = head(&beta);
    let output = cli(&fixture, &beta, &["registry", "publish"]);
    redacted(&output);
    failure(&output);
    let reason = text(&output).to_lowercase();
    assert!(
        reason.contains("base") || reason.contains("generation") || reason.contains("import"),
        "unbased edits need import/reconcile remediation"
    );
    assert_eq!(head(&beta), before);
    assert_eq!(
        fs::read(beta.path("shared").join("knowledge.md")).unwrap(),
        b"unbased beta edit"
    );
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
}

#[test]
fn joined_credential_holder_can_publish_from_copied_current_base_without_takeover() {
    let fixture = initialized(8);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 8);
    ok(&fixture, &beta, &["setup"]);
    author_current(&fixture, &beta);
    write(
        beta.path("shared").join("knowledge.md"),
        b"beta authored current generation",
    );
    ok(&fixture, &beta, &["registry", "publish"]);
    ok(&fixture, &fixture.alpha, &["sync"]);
    let current = active(&fixture, &fixture.alpha);
    assert_eq!(
        fs::read(current.join("knowledge.md")).unwrap(),
        b"beta authored current generation"
    );
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into()])
    );
}

fn pull_to(fixture: &Fixture, caller: &Bot, peer: &str, name: &str) -> (Command, PathBuf) {
    let destination = fixture.temp.path().join(name);
    (
        fixture.cli_command(
            caller,
            &["pull", peer, "--destination", destination.to_str().unwrap()],
        ),
        destination,
    )
}

#[test]
fn credential_only_pull_allows_an_unregistered_caller() {
    let fixture = initialized(8);
    ok(&fixture, &fixture.alpha, &["sync"]);
    let mut caller = fixture.bot("beta", "beta-unregistered");
    machine(&mut caller, 8);
    let (mut command, destination) = pull_to(&fixture, &caller, "alpha", "unregistered-pull");
    command.env_remove("TEST_HUB_BETA_PASSWORD");
    let output = Running::start(command).finish();
    redacted(&output);
    success(&output);
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
    assert_eq!(
        roster(&active(&fixture, &fixture.alpha)),
        BTreeSet::from(["alpha".into()])
    );
    assert!(!Path::new(caller.repository("bot")).join("config").exists());
}

#[test]
fn credential_only_pull_allows_private_peer_without_shared_acl() {
    let mut fixture = Fixture::new();
    machine(&mut fixture.alpha, 8);
    fixture.alpha.config["sharing"]["bot_data"] = json!(false);
    fixture.alpha.save();
    let mut caller = fixture.bot("beta", "beta-1");
    machine(&mut caller, 8);
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &caller]);
    ok(&fixture, &fixture.alpha, &["setup"]);
    ok(&fixture, &fixture.alpha, &["sync"]);
    ok(&fixture, &caller, &["setup"]);
    let (command, destination) = pull_to(&fixture, &caller, "alpha", "private-pull");
    let output = Running::start(command).finish();
    redacted(&output);
    success(&output);
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
}

#[test]
fn pull_rejects_missing_peer_or_shared_credentials_and_preserves_source() {
    let fixture = Fixture::new();
    let caller = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &caller]);
    ok(&fixture, &fixture.alpha, &["setup"]);
    ok(&fixture, &fixture.alpha, &["sync"]);
    ok(&fixture, &caller, &["setup"]);
    // Positive control prevents an unrelated admission failure from satisfying
    // the two absent-credential cases.
    let (command, destination) = pull_to(&fixture, &caller, "alpha", "pull-control");
    let output = Running::start(command).finish();
    redacted(&output);
    success(&output);
    assert_eq!(
        fs::read(destination.join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
    for (name, absent) in [
        ("missing-peer", "TEST_HUB_ALPHA_PASSWORD"),
        ("missing-shared", "TEST_HUB_SHARED_PASSWORD"),
    ] {
        let (mut command, destination) = pull_to(&fixture, &caller, "alpha", name);
        command.env_remove(absent);
        let output = Running::start(command).finish();
        redacted(&output);
        failure(&output);
        assert!(!destination.exists());
    }
    assert_eq!(
        fs::read(fixture.alpha.path("owned").join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
}

#[test]
fn pull_rejects_wrong_hub_bot_or_runtime_source_identity() {
    for (name, hub, id, runtime) in [
        ("wrong-hub", "other", "alpha", "alpha-1"),
        ("wrong-bot", "default", "gamma", "alpha-1"),
        ("wrong-runtime", "default", "alpha", "superseded-runtime"),
    ] {
        let fixture = Fixture::new();
        let caller = fixture.bot("beta", "beta-1");
        fixture.registry(&fixture.alpha, &[&fixture.alpha, &caller]);
        ok(&fixture, &fixture.alpha, &["setup"]);
        ok(&fixture, &caller, &["setup"]);
        backup_tree(
            &fixture,
            &fixture.alpha,
            "bot",
            &fixture.alpha.path("owned"),
            (id, runtime, hub),
            1_700_000_000,
        );
        let (command, destination) = pull_to(&fixture, &caller, "alpha", name);
        let output = Running::start(command).finish();
        redacted(&output);
        assert!(!output.status.success(), "pull accepted {name}");
        assert!(
            !destination.exists(),
            "wrong-source pull must not publish files"
        );
    }
}

#[test]
fn nonowner_setup_automatically_migrates_v1_and_preserves_history_and_private_metadata() {
    let fixture = Fixture::new();
    let mut private = fixture.bot("beta", "beta-private");
    private.config["sharing"]["bot_data"] = json!(false);
    private.save();
    let (_, history) = legacy(&fixture, &[&fixture.alpha, &private]);
    let private_before = private.entry();
    let mut joiner = fixture.bot("gamma", "gamma-1");
    machine(&mut joiner, 8);
    joiner.config["repositories"]["bot"]["password_env"] = json!("TEST_HUB_GAMMA_PASSWORD");
    joiner.save();
    let mut command = fixture.cli_command(&joiner, &["setup"]);
    command
        .env("TEST_HUB_GAMMA_PASSWORD", "synthetic-gamma-only-password")
        .env_remove("TEST_HUB_BETA_PASSWORD");
    let output = Running::start(command).finish();
    redacted(&output);
    assert!(
        !text(&output).contains("synthetic-gamma-only-password"),
        "output leaked an injected credential"
    );
    success(&output);
    let current = active(&fixture, &joiner);
    let value = registry(&current);
    assert_eq!(value["version"], 2);
    assert!(value.get("owner").is_none());
    assert_eq!(value["max_bots"], 8);
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into(), "gamma".into()])
    );
    assert_eq!(
        value["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bot| bot["id"] == "beta")
            .unwrap(),
        &private_before
    );
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
    assert_history_preserved(&current, &history);
}

#[test]
fn v1_low_migration_cap_fails_before_any_incompatible_fence() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    let (original, _) = legacy(&fixture, &[&fixture.alpha, &beta]);
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let mut joiner = fixture.bot("gamma", "gamma-low-cap");
    machine(&mut joiner, 1);
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    assert_unfenced(&fixture, &original);
    assert_eq!(
        snapshot_ids(&fixture, &fixture.alpha),
        before,
        "low cap is known before any migration writes"
    );
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["check"],
    );
}

#[test]
fn v1_corrupt_semantic_history_fails_before_fence_and_valid_legacy_source_remains() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let mut corrupt: Value = serde_json::from_slice(
        &fs::read(fixture.alpha.path("shared").join("registry-history.json")).unwrap(),
    )
    .unwrap();
    corrupt["repository_ids"] = json!(["not-a-master-key-id"]);
    write(
        fixture.alpha.path("shared").join("registry-history.json"),
        serde_json::to_vec(&corrupt).unwrap(),
    );
    let bad = backup_tree(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        ("alpha", "alpha-1", "default"),
        1_700_000_100,
    );
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let joiner = fixture.bot("beta", "beta-1");
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    assert_unfenced(&fixture, &original);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
    // Remove the deliberately invalid snapshot normally; the previously usable
    // source must still pass v1 namespace and restic integrity checks.
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["forget", &bad],
    );
    assert_unfenced(&fixture, &original);
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["check"],
    );
}

#[test]
fn v1_legacy_owner_tag_mismatch_is_rejected_before_incompatible_fence() {
    for (id, runtime) in [("beta", "alpha-1"), ("alpha", "wrong-runtime")] {
        let fixture = Fixture::new();
        let (original, _) = legacy(&fixture, &[&fixture.alpha]);
        backup_tree(
            &fixture,
            &fixture.alpha,
            "shared",
            &fixture.alpha.path("shared"),
            (id, runtime, "default"),
            1_700_000_100,
        );
        let before = snapshot_ids(&fixture, &fixture.alpha);
        let joiner = fixture.bot("beta", "beta-1");
        let output = cli(&fixture, &joiner, &["setup"]);
        redacted(&output);
        failure(&output);
        assert_unfenced(&fixture, &original);
        assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
    }
}

#[test]
fn failed_setup_has_offline_actionable_status_without_raw_stderr_or_credentials() {
    let mut fixture = Fixture::new();
    machine(&mut fixture.alpha, 8);
    let tool = wrapper(&mut fixture.alpha, "fail-snapshots");
    let output = cli(&fixture, &fixture.alpha, &["setup"]);
    redacted(&output);
    failure(&output);
    let calls = fs::read(tool.join("calls")).unwrap();
    write(tool.join("mode"), b"offline");
    let offline = status(&fixture, &fixture.alpha);
    assert_eq!(
        fs::read(tool.join("calls")).unwrap(),
        calls,
        "status must not launch storage children"
    );
    let failure = &offline["failure"];
    assert_eq!(failure["operation"], "setup");
    let reason = failure["reason"].as_str().unwrap().to_lowercase();
    assert!(
        reason.contains("password") || reason.contains("auth") || reason.contains("credential"),
        "offline reason must explain the injected authentication failure"
    );
    let rendered = serde_json::to_string(&offline).unwrap();
    assert!(!rendered.contains("ownerless-raw-stderr-sentinel"));
    for secret in [SHARED_PASSWORD, BOT_PASSWORD, PEER_PASSWORD] {
        assert!(!rendered.contains(secret));
    }
    assert!(
        offline["remediation"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty()),
        "status must explain a next action"
    );
    let persisted = fs::read_to_string(fixture.alpha.path("state").join("status.json")).unwrap();
    assert!(!persisted.contains("ownerless-raw-stderr-sentinel"));
    for secret in [SHARED_PASSWORD, BOT_PASSWORD, PEER_PASSWORD] {
        assert!(!persisted.contains(secret));
    }
    let persisted: Value = serde_json::from_str(&persisted).unwrap();
    assert_eq!(persisted["failures"]["setup"]["operation"], "setup");
    assert_eq!(persisted["failures"]["setup"]["reason"], failure["reason"]);
    write(tool.join("mode"), b"pass");
    ok(&fixture, &fixture.alpha, &["setup"]);
    assert!(
        status(&fixture, &fixture.alpha)["failure"].is_null(),
        "successful retry clears the matching failure"
    );
    let persisted: Value =
        serde_json::from_slice(&fs::read(fixture.alpha.path("state").join("status.json")).unwrap())
            .unwrap();
    assert!(
        persisted["failures"]
            .as_object()
            .unwrap()
            .get("setup")
            .is_none()
    );
}

#[test]
fn raw_v1_fixture_control_is_usable_by_registered_runtime_with_intact_history() {
    let fixture = Fixture::new();
    let (_, history) = legacy(&fixture, &[&fixture.alpha]);
    assert_eq!(registry(&fixture.alpha.path("shared"))["version"], 1);
    ok(&fixture, &fixture.alpha, &["setup"]);
    let current = active(&fixture, &fixture.alpha);
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
    assert_history_preserved(&current, &history);
}

#[test]
fn explicit_default_decision_is_preserved_byte_for_byte_and_joiner_cannot_replace_it() {
    let mut fixture = Fixture::new();
    machine(&mut fixture.alpha, 8);
    let remote = decision_yaml(&fixture.alpha, true);
    write(fixture.alpha.path("shared").join("decisions.yaml"), &remote);
    ok(&fixture, &fixture.alpha, &["setup"]);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 8);
    write(
        beta.path("shared").join("decisions.yaml"),
        decision_yaml(&beta, false),
    );
    ok(&fixture, &beta, &["setup"]);
    assert_eq!(
        fs::read(active(&fixture, &beta).join("decisions.yaml")).unwrap(),
        remote.as_bytes()
    );
    let value: Value = serde_saphyr::from_str(&remote).unwrap();
    assert_eq!(value["caveman"]["enabled"], true);
    assert_eq!(value["caveman"]["basis"], "user_decision");
}

#[test]
fn v1_migration_fills_missing_remote_decision_from_valid_scoped_local_record() {
    let fixture = Fixture::new();
    legacy(&fixture, &[&fixture.alpha]);
    let beta = fixture.bot("beta", "beta-1");
    let supplement = decision_yaml(&beta, true);
    write(beta.path("shared").join("decisions.yaml"), &supplement);
    ok(&fixture, &beta, &["setup"]);
    let current = active(&fixture, &beta);
    let adopted: Value =
        serde_saphyr::from_slice(&fs::read(current.join("decisions.yaml")).unwrap()).unwrap();
    let expected: Value = serde_saphyr::from_str(&supplement).unwrap();
    assert_eq!(adopted, expected);
}

#[test]
fn v1_migration_never_overwrites_an_existing_remote_decision_with_local_record() {
    let fixture = Fixture::new();
    let remote = decision_yaml(&fixture.alpha, true);
    write(fixture.alpha.path("shared").join("decisions.yaml"), &remote);
    legacy(&fixture, &[&fixture.alpha]);
    let beta = fixture.bot("beta", "beta-1");
    write(
        beta.path("shared").join("decisions.yaml"),
        decision_yaml(&beta, false),
    );
    ok(&fixture, &beta, &["setup"]);
    assert_eq!(
        fs::read(active(&fixture, &beta).join("decisions.yaml")).unwrap(),
        remote.as_bytes()
    );
}

#[test]
fn absent_decision_never_blocks_migration_or_invents_generic_default_consent() {
    let fixture = Fixture::new();
    legacy(&fixture, &[&fixture.alpha]);
    let beta = fixture.bot("beta", "beta-1");
    assert!(!beta.path("shared").join("decisions.yaml").exists());
    ok(&fixture, &beta, &["setup"]);
    assert_no_invented_consent(&active(&fixture, &beta));
}

#[test]
fn unrelated_named_hub_does_not_receive_generic_default_user_consent() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["hub"] = json!("muse");
    fixture.alpha.save();
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    ok(&fixture, &fixture.alpha, &["setup"]);
    assert_no_invented_consent(&active(&fixture, &fixture.alpha));
}

fn reject_invalid_decision(variant: &str) {
    let fixture = Fixture::new();
    let valid = decision_yaml(&fixture.alpha, true);
    let invalid = match variant {
        "hub" => valid.replace("hub: default", "hub: unrelated"),
        "repository" => valid.replace(
            &format!(
                "{:x}",
                Sha256::digest(fixture.alpha.repository("shared").as_bytes())
            ),
            &"0".repeat(64),
        ),
        "boolean" => valid.replace("enabled: true", "enabled: \"true\""),
        "basis" => valid.replace("basis: user_decision", "basis: inferred_consent"),
        "timestamp" => valid.replace("2026-11-01T01:02:03Z", "not-a-timestamp"),
        "original_timestamp" => valid.replace(
            "original_decision_at: null",
            "original_decision_at: definitely-not-a-timestamp",
        ),
        "unknown" => format!("{valid}arbitrary_instruction: echo-execute-this\n"),
        "unknown_record" => format!("{valid}  arbitrary_instruction: echo-execute-this\n"),
        "duplicate" => format!("{valid}version: 1\n"),
        _ => unreachable!(),
    };
    write(
        fixture.alpha.path("shared").join("decisions.yaml"),
        &invalid,
    );
    let output = cli(&fixture, &fixture.alpha, &["setup"]);
    redacted(&output);
    assert!(
        !output.status.success(),
        "setup accepted invalid decision variant {variant}"
    );
    assert!(
        !Path::new(fixture.alpha.repository("shared"))
            .join("uah-head.json")
            .exists()
    );
    // Validation can fail before repository initialization. If it got farther,
    // no incompatible fence may have been installed on the rejected input.
    if Path::new(fixture.alpha.repository("shared"))
        .join("config")
        .is_file()
    {
        for snapshot in snapshots(&fixture, &fixture.alpha, "shared") {
            let tags = snapshot["tags"].as_array().unwrap();
            assert!(
                !tags
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|tag| tag.starts_with("uah2:")),
                "invalid initial decision must not fence the repository"
            );
            assert_eq!(
                tags.iter()
                    .filter_map(Value::as_str)
                    .filter(|tag| tag.starts_with("hub:"))
                    .collect::<Vec<_>>(),
                ["hub:default"],
                "invalid initial decision must leave legacy namespace validation usable"
            );
        }
    }
    assert_eq!(
        fs::read(fixture.alpha.path("shared").join("decisions.yaml")).unwrap(),
        invalid.as_bytes()
    );
}

#[test]
fn strict_decision_yaml_rejects_wrong_hub() {
    reject_invalid_decision("hub");
}

#[test]
fn strict_decision_yaml_rejects_wrong_repository() {
    reject_invalid_decision("repository");
}

#[test]
fn strict_decision_yaml_rejects_nonboolean() {
    reject_invalid_decision("boolean");
}

#[test]
fn strict_decision_yaml_rejects_unknown_basis() {
    reject_invalid_decision("basis");
}

#[test]
fn strict_decision_yaml_rejects_invalid_timestamp() {
    reject_invalid_decision("timestamp");
}

#[test]
fn strict_decision_yaml_rejects_unknown_fields() {
    reject_invalid_decision("unknown");
}

#[test]
fn strict_decision_yaml_rejects_duplicate_fields() {
    reject_invalid_decision("duplicate");
}

#[test]
fn strict_decision_yaml_rejects_unknown_record_fields() {
    reject_invalid_decision("unknown_record");
}

#[test]
fn strict_decision_yaml_rejects_invalid_original_timestamp() {
    reject_invalid_decision("original_timestamp");
}

#[test]
fn v1_invalid_local_decision_binding_is_rejected_before_incompatible_fence() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let beta = fixture.bot("beta", "beta-1");
    let invalid = decision_yaml(&beta, true).replace(
        &format!("{:x}", Sha256::digest(beta.repository("shared").as_bytes())),
        &"f".repeat(64),
    );
    write(beta.path("shared").join("decisions.yaml"), &invalid);
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let output = cli(&fixture, &beta, &["setup"]);
    redacted(&output);
    failure(&output);
    assert!(
        text(&output).to_lowercase().contains("decision"),
        "a wrong decision target must be diagnosed as a decision error"
    );
    assert_unfenced(&fixture, &original);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
}

#[test]
fn migration_rejects_config_swap_to_cloned_repository_before_capture_or_fence() {
    let fixture = Fixture::new();
    let (source, history) = legacy(&fixture, &[&fixture.alpha]);
    let copied_repository = fixture.temp.path().join("repositories/shared-copy");
    copy_tree(
        Path::new(fixture.alpha.repository("shared")),
        &copied_repository,
    );
    let mut copied = Bot {
        file: fixture.alpha.file.clone(),
        config: fixture.alpha.config.clone(),
    };
    copied.config["repositories"]["shared"]["url"] = json!(copied_repository);
    let original_a = snapshots(&fixture, &fixture.alpha, "shared");
    let original_b = snapshots(&fixture, &copied, "shared");
    assert_eq!(
        original_a, original_b,
        "quiescent clone must preserve the exact source snapshot ID"
    );
    assert_eq!(original_a[0]["id"], source);

    let mut joiner = fixture.bot("beta", "beta-config-swap");
    machine(&mut joiner, 8);
    let tool = wrapper(&mut joiner, "swap-config-before-fence");
    let original_config = fs::read(&joiner.file).unwrap();
    let mut replacement = joiner.config.clone();
    replacement["repositories"]["shared"]["url"] = json!(copied_repository);
    write(
        tool.join("replacement-config"),
        serde_json::to_vec_pretty(&replacement).unwrap(),
    );
    write(tool.join("config-path"), joiner.file.to_str().unwrap());

    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    assert!(
        tool.join("config-swapped").is_file(),
        "test must reach the fence/helper boundary: {}",
        text(&output)
    );
    let after_a = snapshots(&fixture, &fixture.alpha, "shared");
    let after_b = snapshots(&fixture, &copied, "shared");
    eprintln!(
        "config-swap source={source}; A before={} after={}; B before={} after={}; setup exit={:?}; {}",
        original_a.len(),
        after_a.len(),
        original_b.len(),
        after_b.len(),
        output.status.code(),
        text(&output)
    );
    for (name, roots) in [("A", &after_a), ("B", &after_b)] {
        let summary: Vec<_> = roots
            .iter()
            .map(|snapshot| json!({"id":snapshot["id"], "tags":snapshot["tags"]}))
            .collect();
        eprintln!("config-swap {name} roots={}", json!(summary));
    }
    failure(&output);
    assert_eq!(
        after_a, original_a,
        "changed config must be rejected before writing a format fence into A"
    );
    assert_eq!(
        after_b, original_b,
        "changed config must be rejected before writing a retained capture into B"
    );
    assert_unfenced(&fixture, &source);
    assert!(!copied_repository.join("uah-head.json").exists());
    assert!(
        !after_b.iter().any(|snapshot| snapshot["tags"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .any(|tag| tag.starts_with("uah2:"))),
        "changed config must leave the cloned repository unfenced"
    );

    // A rejected race must leave the original hub usable without manual repair.
    fs::write(&joiner.file, original_config).unwrap();
    write(tool.join("mode"), b"pass");
    ok(&fixture, &joiner, &["setup"]);
    let current = active(&fixture, &joiner);
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
    assert_history_preserved(&current, &history);
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into()])
    );
    assert_eq!(snapshots(&fixture, &copied, "shared"), original_b);
}

// The wrapper stops only after the real outer --stdin-from-command backup
// returns successfully. At that barrier the fence is durable and its native
// append lock has been released; no test lock or forced unlock is involved.
#[test]
fn new_hub_restarts_after_durable_null_source_fence_without_extra_bot_backups() {
    let mut fixture = Fixture::new();
    machine(&mut fixture.alpha, 3);
    write(fixture.alpha.path("shared").join("knowledge.md"), KNOWLEDGE);
    let tool = wrapper(&mut fixture.alpha, "durable-fence");
    let mut initializing = Running::start(fixture.cli_command(&fixture.alpha, &["setup"]));
    initializing.reached(&tool.join("ready"));
    assert!(
        !Path::new(fixture.alpha.repository("shared"))
            .join("uah-head.json")
            .exists()
    );
    let all = snapshots(&fixture, &fixture.alpha, "shared");
    assert_eq!(
        all.len(),
        1,
        "empty initialization needs a control root, no legacy capture"
    );
    let fence_id = all[0]["id"].as_str().unwrap().to_owned();
    let tags = all[0]["tags"].as_array().unwrap();
    for required in [
        "uah2:kind:format-fence",
        "uah2:format:2",
        "uah2:hub:default",
    ] {
        assert!(tags.contains(&json!(required)));
    }
    assert!(
        !tags
            .iter()
            .filter_map(Value::as_str)
            .any(|tag| tag.starts_with("hub:"))
    );
    let restored = fixture.temp.path().join("interrupted-empty-fence");
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &[
            "restore",
            &fence_id,
            "--target",
            restored.to_str().unwrap(),
            "--verify",
        ],
    );
    let payload: Value =
        serde_json::from_slice(&fs::read(restored.join("format-fence.json")).unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"type":"utsurigi-agent-hub/format-fence", "format":2, "hub":"default", "source":null, "capture":null})
    );
    drop(initializing);
    write(tool.join("mode"), b"pass");
    ok(&fixture, &fixture.alpha, &["setup"]);
    assert_eq!(head(&fixture.alpha)["revision"], 1);
    assert_eq!(
        fs::read(active(&fixture, &fixture.alpha).join("knowledge.md")).unwrap(),
        KNOWLEDGE
    );
    let after = snapshots(&fixture, &fixture.alpha, "shared");
    assert!(
        after.contains(&all[0]),
        "resume must retain the original null-source fence"
    );
    assert_eq!(
        after
            .iter()
            .filter(|snapshot| snapshot["tags"]
                .as_array()
                .unwrap()
                .contains(&json!("uah2:kind:format-fence")))
            .count(),
        1
    );
    assert!(snapshots(&fixture, &fixture.alpha, "bot").is_empty());
    ok(&fixture, &fixture.alpha, &["sync"]);
    assert_eq!(snapshots(&fixture, &fixture.alpha, "bot").len(), 1);
    let protected = snapshot_ids(&fixture, &fixture.alpha);
    ok(&fixture, &fixture.alpha, &["prune"]);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), protected);
}

#[test]
fn restart_after_durable_fence_uses_retained_capture_after_original_source_is_forgotten() {
    let fixture = Fixture::new();
    let (original, history) = legacy(&fixture, &[&fixture.alpha]);
    let mut beta = fixture.bot("beta", "beta-1");
    machine(&mut beta, 8);
    let tool = wrapper(&mut beta, "durable-fence");
    let mut migrating = Running::start(fixture.cli_command(&beta, &["setup"]));
    migrating.reached(&tool.join("ready"));
    let all = snapshots(&fixture, &fixture.alpha, "shared");
    let fence = all
        .iter()
        .find(|snapshot| {
            !snapshot["tags"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .any(|tag| tag.starts_with("hub:"))
        })
        .expect("durable fence must be incompatible with legacy namespace validation");
    let fence_id = fence["id"].as_str().unwrap().to_owned();
    let fence_tags = fence["tags"].as_array().unwrap();
    for required in [
        "uah2:kind:format-fence",
        "uah2:format:2",
        "uah2:hub:default",
    ] {
        assert!(fence_tags.contains(&json!(required)));
    }
    let capture = all
        .iter()
        .find(|snapshot| {
            let tags = snapshot["tags"].as_array().unwrap();
            snapshot["id"] != original
                && snapshot["id"] != fence_id
                && tags.contains(&json!("hub:default"))
                && !tags.contains(&json!("kind:shared"))
        })
        .expect("the fence must retain an actual snapshot of the selected v1 tree");
    let capture_id = capture["id"].as_str().unwrap().to_owned();
    let capture_tags = capture["tags"].as_array().unwrap();
    assert!(capture_tags.contains(&json!("kind:migration-capture")));
    assert!(
        !capture_tags
            .iter()
            .filter_map(Value::as_str)
            .any(|tag| tag.starts_with("completed:"))
    );
    let fence_tree = fixture.temp.path().join("durable-fence");
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &[
            "restore",
            &fence_id,
            "--target",
            fence_tree.to_str().unwrap(),
            "--verify",
        ],
    );
    let payload: Value =
        serde_json::from_slice(&fs::read(fence_tree.join("format-fence.json")).unwrap()).unwrap();
    assert_eq!(
        payload,
        json!({"type":"utsurigi-agent-hub/format-fence", "format":2, "hub":"default", "source":original, "capture":capture_id})
    );
    let retained = fixture.temp.path().join("retained-before-crash");
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &[
            "restore",
            &capture_id,
            "--target",
            retained.to_str().unwrap(),
            "--verify",
        ],
    );
    assert_eq!(fs::read(retained.join("knowledge.md")).unwrap(), KNOWLEDGE);
    for file in ["registry.yaml", "registry-history.json", "knowledge.md"] {
        assert_eq!(
            fs::read(retained.join(file)).unwrap(),
            fs::read(fixture.alpha.path("shared").join(file)).unwrap(),
            "capture must retain the original bytes of {file}"
        );
    }
    assert_history_preserved(&retained, &history);
    assert!(
        !Path::new(beta.repository("shared"))
            .join("uah-head.json")
            .exists(),
        "the crash barrier precedes head creation"
    );
    drop(migrating);
    write(tool.join("mode"), b"pass");
    // Real restic GC removes the original snapshot and unreferenced content.
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["forget", &original, "--prune"],
    );
    let kept = snapshot_ids(&fixture, &fixture.alpha);
    assert!(!kept.contains(&original));
    assert!(kept.contains(&capture_id));
    assert!(kept.contains(&fence_id));
    // A delayed legacy publisher may leave a newer complete v1 snapshot. A
    // restart must follow the fenced capture, never re-select that latest tree.
    write(
        fixture.alpha.path("shared").join("knowledge.md"),
        b"late legacy snapshot must not become current",
    );
    let late = backup_tree(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        ("alpha", "alpha-1", "default"),
        2_000_000_000,
    );
    ok(&fixture, &beta, &["setup"]);
    let current = active(&fixture, &beta);
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
    assert_history_preserved(&current, &history);
    assert_eq!(
        roster(&current),
        BTreeSet::from(["alpha".into(), "beta".into()])
    );
    let protected = snapshot_ids(&fixture, &fixture.alpha);
    assert!(protected.contains(&late));
    ok(&fixture, &beta, &["prune"]);
    assert_eq!(
        snapshot_ids(&fixture, &fixture.alpha),
        protected,
        "shared control/capture/current/candidates remain protected"
    );
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["check"],
    );
}

fn supplied_legacy_command(fixture: &Fixture, bot: &Bot, args: &[&str]) -> Command {
    let binary = std::env::var_os("UTSURIGI_AGENT_HUB_LEGACY_BINARY")
        .expect("this ignored local compatibility probe requires UTSURIGI_AGENT_HUB_LEGACY_BINARY");
    let bytes = fs::read(&binary).expect("supplied legacy executable must be readable");
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "3e93d367b59d83c56d9fb36c7fda3bc758c51ffb44778ab4b5eac16b5ad54c61",
        "the compatibility probe requires the preserved preimplementation binary"
    );
    let base = fixture.cli_command(bot, args);
    let mut command = Command::new(binary);
    command
        .args(base.get_args())
        .env_clear()
        .current_dir(base.get_current_dir().unwrap());
    for (name, value) in base.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    command
}

#[test]
#[ignore = "local compatibility probe; requires UTSURIGI_AGENT_HUB_LEGACY_BINARY"]
fn supplied_legacy_writer_is_rejected_after_automatic_nonowner_migration() {
    let fixture = Fixture::new();
    legacy(&fixture, &[&fixture.alpha]);
    let before = Running::start(supplied_legacy_command(
        &fixture,
        &fixture.alpha,
        &["setup"],
    ))
    .finish();
    redacted(&before);
    success(&before);
    let beta = fixture.bot("beta", "beta-1");
    ok(&fixture, &beta, &["setup"]);
    let accepted = head(&beta);
    let output = Running::start(supplied_legacy_command(
        &fixture,
        &fixture.alpha,
        &["registry", "publish"],
    ))
    .finish();
    redacted(&output);
    failure(&output);
    assert!(
        text(&output).to_lowercase().contains("hub"),
        "unchanged v1 binary must reject the incompatible namespace fence"
    );
    assert_eq!(head(&beta), accepted);
    assert_eq!(
        fs::read(fixture.alpha.path("owned").join("notes/日本語.md")).unwrap(),
        PAYLOAD
    );
}

#[test]
#[ignore = "local compatibility probe; requires UTSURIGI_AGENT_HUB_LEGACY_BINARY"]
fn supplied_legacy_hub_stays_usable_after_new_migrator_rejects_low_cap_and_corrupt_history() {
    let fixture = Fixture::new();
    let beta = fixture.bot("beta", "beta-1");
    let (original, history) = legacy(&fixture, &[&fixture.alpha, &beta]);
    let old_setup = || {
        let output = Running::start(supplied_legacy_command(
            &fixture,
            &fixture.alpha,
            &["setup"],
        ))
        .finish();
        redacted(&output);
        success(&output);
    };
    old_setup();
    let mut joiner = fixture.bot("gamma", "gamma-low-cap");
    machine(&mut joiner, 1);
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    assert_unfenced(&fixture, &original);
    old_setup();
    write(
        fixture.alpha.path("shared").join("registry-history.json"),
        b"{\"version\":1,\"repository_ids\":[]}",
    );
    let bad = backup_tree(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        ("alpha", "alpha-1", "default"),
        1_700_000_100,
    );
    machine(&mut joiner, 8);
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    assert_unfenced(&fixture, &original);
    raw(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        &["forget", &bad],
    );
    write(
        fixture.alpha.path("shared").join("registry-history.json"),
        serde_json::to_vec(&history).unwrap(),
    );
    old_setup();
}

#[test]
fn credential_holder_outside_roster_can_publish_a_copied_current_authoring_base() {
    let fixture = initialized(8);
    let mut caller = fixture.bot("beta", "beta-author");
    machine(&mut caller, 8);
    copy_tree(&active(&fixture, &fixture.alpha), &caller.path("shared"));
    fs::remove_file(caller.path("shared").join("registry-history.json")).unwrap();
    write(
        caller.path("shared").join("knowledge.md"),
        b"credential-held unregistered publication",
    );
    let mut command = fixture.cli_command(&caller, &["registry", "publish"]);
    command.env_remove("TEST_HUB_BETA_PASSWORD");
    command.env_remove("TEST_HUB_ALPHA_PASSWORD");
    let output = Running::start(command).finish();
    redacted(&output);
    success(&output);
    ok(&fixture, &fixture.alpha, &["sync"]);
    let current = active(&fixture, &fixture.alpha);
    assert_eq!(
        fs::read(current.join("knowledge.md")).unwrap(),
        b"credential-held unregistered publication"
    );
    assert_eq!(roster(&current), BTreeSet::from(["alpha".into()]));
    assert!(!Path::new(caller.repository("bot")).join("config").exists());
}

#[test]
fn v1_full_fleet_refuses_new_registration_before_fence() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let mut joiner = fixture.bot("beta", "beta-full");
    machine(&mut joiner, 1);
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    let reason = text(&output).to_lowercase();
    assert!(
        reason.contains("max_bots")
            || reason.contains("cap")
            || reason.contains("limit")
            || reason.contains("fleet"),
        "a full legacy fleet needs a shared-cap explanation before fencing"
    );
    assert_unfenced(&fixture, &original);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
}

#[test]
fn v1_missing_own_or_shared_credentials_fails_before_fence() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let joiner = fixture.bot("beta", "beta-1");
    let before = snapshot_ids(&fixture, &fixture.alpha);
    for absent in ["TEST_HUB_BETA_PASSWORD", "TEST_HUB_SHARED_PASSWORD"] {
        let mut command = fixture.cli_command(&joiner, &["setup"]);
        command.env_remove(absent);
        let output = Running::start(command).finish();
        redacted(&output);
        failure(&output);
        assert_unfenced(&fixture, &original);
        assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
    }
}

#[test]
fn v1_private_registration_cannot_reuse_retired_shared_identity_before_fence() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let mut joiner = fixture.bot("beta", "beta-private");
    joiner.config["sharing"]["bot_data"] = json!(false);
    joiner.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("retired-repository"));
    joiner.save();
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let output = cli(&fixture, &joiner, &["setup"]);
    redacted(&output);
    failure(&output);
    let reason = text(&output).to_lowercase();
    assert!(
        reason.contains("private") || reason.contains("master key"),
        "private-history rejection must explain the data separation conflict"
    );
    assert_unfenced(&fixture, &original);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
}

#[test]
fn v1_migration_does_not_require_an_optional_public_peer_password() {
    let fixture = Fixture::new();
    let (_, history) = legacy(&fixture, &[&fixture.alpha]);
    let joiner = fixture.bot("beta", "beta-1");
    let mut command = fixture.cli_command(&joiner, &["setup"]);
    command.env_remove("TEST_HUB_ALPHA_PASSWORD");
    let output = Running::start(command).finish();
    redacted(&output);
    success(&output);
    let current = active(&fixture, &joiner);
    assert_history_preserved(&current, &history);
    assert_eq!(fs::read(current.join("knowledge.md")).unwrap(), KNOWLEDGE);
}

#[test]
fn v1_budget_hold_fails_before_incompatible_fence_using_only_local_storage() {
    let fixture = Fixture::new();
    let (original, _) = legacy(&fixture, &[&fixture.alpha]);
    let shared_url = "s3:https://ownerless-acceptance-42.r2.cloudflarestorage.com/budget/shared";
    let bot_url = "s3:https://ownerless-acceptance-42.r2.cloudflarestorage.com/budget/alpha";
    let mut source_registry = registry(&fixture.alpha.path("shared"));
    source_registry["bots"][0]["repository"] = json!(bot_url);
    write(
        fixture.alpha.path("shared").join("registry.yaml"),
        serde_json::to_vec(&source_registry).unwrap(),
    );
    let path = fixture.alpha.path("shared").join("registry-history.json");
    let mut history: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    history["repository_urls"] = json!([shared_url, bot_url]);
    write(path, serde_json::to_vec(&history).unwrap());
    backup_tree(
        &fixture,
        &fixture.alpha,
        "shared",
        &fixture.alpha.path("shared"),
        ("alpha", "alpha-1", "default"),
        1_700_000_100,
    );
    let before = snapshot_ids(&fixture, &fixture.alpha);
    let mut caller = fixture.bot("alpha", "alpha-budget");
    machine(&mut caller, 8);
    caller.config["bot"]["runtime_id"] = json!("alpha-1");
    caller.config["storage"]["provider"] = json!("r2");
    caller.config["repositories"]["shared"]["url"] = json!(shared_url);
    caller.config["repositories"]["bot"]["url"] = json!(bot_url);
    caller.save();
    let tool = wrapper(&mut caller, "pass");
    write(tool.join("repo-shared"), fixture.alpha.repository("shared"));
    write(tool.join("repo-bot"), fixture.alpha.repository("bot"));
    let bin = fixture.temp.path().join("fake-curl-bin");
    fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    fs::write(&curl, include_str!("fixtures/ownerless-curl-absent.py")).unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
    let search_path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let mut command = fixture.cli_command(&caller, &["setup"]);
    command
        .env("PATH", search_path)
        .env("AWS_ACCESS_KEY_ID", "synthetic-budget-access")
        .env("AWS_SECRET_ACCESS_KEY", "synthetic-budget-secret");
    let output = Running::start(command).finish();
    redacted(&output);
    failure(&output);
    let reason = text(&output).to_lowercase();
    assert!(
        reason.contains("budget") || reason.contains("attestation"),
        "budget validation must fail before fencing"
    );
    assert_unfenced(&fixture, &original);
    assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
}

#[test]
fn v1_missing_history_or_invalid_registry_fails_before_incompatible_fence() {
    for variant in ["missing-history", "invalid-registry"] {
        let fixture = Fixture::new();
        let (original, _) = legacy(&fixture, &[&fixture.alpha]);
        if variant == "missing-history" {
            fs::remove_file(fixture.alpha.path("shared").join("registry-history.json")).unwrap();
        } else {
            let mut invalid = registry(&fixture.alpha.path("shared"));
            invalid["owner"]["runtime_id"] = json!("not-a-current-runtime");
            write(
                fixture.alpha.path("shared").join("registry.yaml"),
                serde_json::to_vec(&invalid).unwrap(),
            );
        }
        let bad = backup_tree(
            &fixture,
            &fixture.alpha,
            "shared",
            &fixture.alpha.path("shared"),
            ("alpha", "alpha-1", "default"),
            1_700_000_100,
        );
        let before = snapshot_ids(&fixture, &fixture.alpha);
        let output = cli(&fixture, &fixture.alpha, &["setup"]);
        redacted(&output);
        failure(&output);
        assert_unfenced(&fixture, &original);
        assert_eq!(snapshot_ids(&fixture, &fixture.alpha), before);
        raw(
            &fixture,
            &fixture.alpha,
            "shared",
            &fixture.alpha.path("shared"),
            &["forget", &bad],
        );
        assert_unfenced(&fixture, &original);
        raw(
            &fixture,
            &fixture.alpha,
            "shared",
            &fixture.alpha.path("shared"),
            &["check"],
        );
    }
}

#[test]
fn v1_existing_remote_envelope_without_record_accepts_only_scoped_local_record() {
    let fixture = Fixture::new();
    let envelope = decision_yaml(&fixture.alpha, true)
        .split("caveman:")
        .next()
        .unwrap()
        .to_owned();
    write(
        fixture.alpha.path("shared").join("decisions.yaml"),
        &envelope,
    );
    legacy(&fixture, &[&fixture.alpha]);
    let beta = fixture.bot("beta", "beta-1");
    let local = decision_yaml(&beta, true);
    write(beta.path("shared").join("decisions.yaml"), &local);
    ok(&fixture, &beta, &["setup"]);
    let adopted: Value = serde_saphyr::from_slice(
        &fs::read(active(&fixture, &beta).join("decisions.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(adopted, serde_saphyr::from_str::<Value>(&local).unwrap());
}
