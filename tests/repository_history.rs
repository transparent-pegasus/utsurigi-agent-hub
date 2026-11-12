#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

const HISTORY: &str = "registry-history.json";
const REVISION: &str = "shared-revision.json";
const PROPOSAL: &str = "proposal.md";
// Today's refusal when a previously shared repository is missing at its registered URL.
const MISSING: &str = "restic bot repository operation failed (exit 10: repository does not exist)";

fn repository_id(fixture: &Fixture, bot: &Bot, repository: &str) -> String {
    let output = fixture.restic(bot, repository, &["cat", "config"]);
    success(&output);
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    config["id"].as_str().unwrap().to_owned()
}

// Reads the accepted head snapshot itself, not a local import of it.
fn head_history(fixture: &Fixture, bot: &Bot) -> Value {
    let snapshot = fixture.current_shared_id(bot);
    let output = fixture.restic(bot, "shared", &["dump", &snapshot, &format!("/{HISTORY}")]);
    success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

// Per-Bot repository IDs exist from history version 2.
fn bot_repository_ids(fixture: &Fixture, bot: &Bot) -> Value {
    let history = head_history(fixture, bot);
    assert_eq!(history["version"], 2, "{history}");
    history["bot_repository_ids"].clone()
}

// The flat set already holds the substitute's ID, so it cannot be refused as unknown.
fn assert_known(fixture: &Fixture, bot: &Bot, repository: &str) {
    let id = repository_id(fixture, bot, repository);
    let history = head_history(fixture, bot);
    assert!(
        history["repository_ids"]
            .as_array()
            .unwrap()
            .contains(&json!(id)),
        "{history}"
    );
}

// Shared repositories may share a password. When the moved Bot shares one with another repository,
// a copy of that repository opens with the Bot's unchanged registry entry, and only its restic ID
// tells it apart.
fn share_password(bot: &mut Bot, source: &Value) {
    for key in ["password_env", "password_ref"] {
        bot.config["repositories"]["bot"][key] = source[key].clone();
    }
    bot.save();
}

// A full move, like a Drive generation restored on another host: a successor runtime of alpha gets
// a byte-identical copy of the shared repository and of `bot_source` at new absolute paths, and
// alpha's old directories are gone.
fn full_move(fixture: &Fixture, bot_source: &str) -> Bot {
    let alpha = &fixture.alpha;
    let mut successor = fixture.bot("alpha", "alpha-2");
    successor.config["repositories"] = alpha.config["repositories"].clone();
    let relocated = fixture.temp.path().join("relocated");
    copy_tree(
        Path::new(alpha.repository("shared")),
        &relocated.join("shared"),
    );
    copy_tree(Path::new(bot_source), &relocated.join("bot"));
    for name in ["shared", "bot"] {
        fs::remove_dir_all(alpha.repository(name)).unwrap();
        successor.config["repositories"][name]["url"] = json!(relocated.join(name));
    }
    successor.save();
    successor
}

fn author_from(source: &Path, bot: &Bot) {
    fs::remove_dir_all(bot.path("shared")).unwrap();
    copy_tree(source, &bot.path("shared"));
    fs::remove_file(bot.path("shared").join(HISTORY)).unwrap();
}

// Accepts the current tree as the next head with a version 1 history: only the four flat sets, as
// every Bot wrote them before per-Bot IDs. The snapshot keeps the head snapshot's native identity.
fn accept_version_1(fixture: &Fixture, bot: &Bot) -> PathBuf {
    let mut head = fixture.shared_head(bot);
    let revision = head["revision"].as_u64().unwrap() + 1;
    let tree = fixture.temp.path().join("version-1-head");
    copy_tree(&fixture.active_shared(bot), &tree);
    let history: Value = serde_json::from_slice(&fs::read(tree.join(HISTORY)).unwrap()).unwrap();
    let legacy = json!({
        "version": 1,
        "repository_urls": history["repository_urls"],
        "password_refs": history["password_refs"],
        "password_envs": history["password_envs"],
        "repository_ids": history["repository_ids"],
    });
    write(tree.join(HISTORY), legacy.to_string());
    let mut record: Value =
        serde_json::from_slice(&fs::read(tree.join(REVISION)).unwrap()).unwrap();
    record["revision"] = json!(revision);
    write(tree.join(REVISION), record.to_string());
    let accepted = fixture
        .snapshots(bot, "shared")
        .into_iter()
        .find(|snapshot| snapshot["id"] == head["snapshot"])
        .unwrap();
    let mut args = vec![
        "backup",
        ".",
        "--json",
        "--host",
        accepted["hostname"].as_str().unwrap(),
    ];
    for tag in accepted["tags"].as_array().unwrap() {
        args.extend(["--tag", tag.as_str().unwrap()]);
    }
    let mut view = Bot {
        file: bot.file.clone(),
        config: bot.config.clone(),
    };
    view.config["paths"]["shared"] = json!(tree);
    let output = fixture.restic(&view, "shared", &args);
    success(&output);
    let snapshot = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|line| line["snapshot_id"].as_str().map(str::to_owned))
        .unwrap();
    head["revision"] = json!(revision);
    head["snapshot"] = json!(snapshot);
    fs::write(
        Path::new(bot.repository("shared")).join("uah-head.json"),
        serde_json::to_vec(&head).unwrap(),
    )
    .unwrap();
    assert_eq!(head_history(fixture, bot), legacy);
    tree
}

fn refused(fixture: &Fixture, bot: &Bot) {
    write(bot.path("shared").join(PROPOSAL), "unpublished proposal");
    let edits = || {
        ["registry.yaml", REVISION, PROPOSAL]
            .map(|name| fs::read(bot.path("shared").join(name)).ok())
    };
    let (head, before) = (fixture.shared_head(bot), edits());
    let output = fixture.cli(bot, &["registry", "publish", "--takeover"]);
    failure(&output);
    assert_eq!(output.status.code(), Some(10), "{}", text(&output));
    assert!(text(&output).contains(MISSING), "{}", text(&output));
    assert_eq!(fixture.shared_head(bot), head);
    assert_eq!(edits(), before);
}

#[test]
fn a_full_move_with_version_2_history_keeps_the_moved_repository_under_its_bot() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    // The hub repository's ID belongs to no Bot.
    assert_eq!(
        bot_repository_ids(&fixture, alpha),
        json!({"alpha": [repository_id(&fixture, alpha, "bot")]})
    );
    let successor = full_move(&fixture, alpha.repository("bot"));
    author_from(&fixture.active_shared(alpha), &successor);
    fixture.registry(&successor, &[&successor]);
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    assert_eq!(
        bot_repository_ids(&fixture, &successor),
        json!({"alpha": [repository_id(&fixture, &successor, "bot")]})
    );
}

#[test]
fn a_copy_of_another_bots_repository_cannot_stand_in_for_a_moved_one() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let mut beta = fixture.bot("beta", "beta-1");
    share_password(&mut beta, &alpha.config["repositories"]["bot"]);
    fixture.ok(&beta, &["setup"]);
    fixture.ok(alpha, &["sync"]);
    let successor = full_move(&fixture, beta.repository("bot"));
    author_from(&fixture.active_shared(alpha), &successor);
    fixture.registry(&successor, &[&successor, &beta]);
    assert_known(&fixture, &successor, "bot");
    refused(&fixture, &successor);
}

#[test]
fn a_copy_of_the_hub_repository_cannot_stand_in_for_a_moved_bot_repository() {
    let mut fixture = Fixture::new();
    let hub = fixture.alpha.config["repositories"]["shared"].clone();
    share_password(&mut fixture.alpha, &hub);
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let successor = full_move(&fixture, alpha.repository("shared"));
    author_from(&fixture.active_shared(alpha), &successor);
    fixture.registry(&successor, &[&successor]);
    // The copy at the Bot path keeps the hub repository's ID.
    assert_known(&fixture, &successor, "shared");
    refused(&fixture, &successor);
}

#[test]
fn a_version_1_history_accepts_a_moved_repository_whose_snapshots_are_the_bots() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    // Attribution by content needs a snapshot of the Bot's own data.
    fixture.ok(alpha, &["sync"]);
    let legacy = accept_version_1(&fixture, alpha);
    let successor = full_move(&fixture, alpha.repository("bot"));
    author_from(&legacy, &successor);
    fixture.registry(&successor, &[&successor]);
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    assert_eq!(
        bot_repository_ids(&fixture, &successor),
        json!({"alpha": [repository_id(&fixture, &successor, "bot")]})
    );
}

#[test]
fn a_version_1_history_refuses_a_copy_of_another_bots_repository() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let mut beta = fixture.bot("beta", "beta-1");
    share_password(&mut beta, &alpha.config["repositories"]["bot"]);
    fixture.ok(&beta, &["setup"]);
    // Beta's snapshots carry bot:beta, so they cannot attribute a copy to alpha.
    fixture.ok(&beta, &["sync"]);
    fixture.ok(alpha, &["sync"]);
    let legacy = accept_version_1(&fixture, alpha);
    let successor = full_move(&fixture, beta.repository("bot"));
    author_from(&legacy, &successor);
    fixture.registry(&successor, &[&successor, &beta]);
    assert_known(&fixture, &successor, "bot");
    refused(&fixture, &successor);
}
