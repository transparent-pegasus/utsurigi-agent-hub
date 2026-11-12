#[allow(dead_code)]
mod common;

use common::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const REVISION: &str = "shared-revision.json";
const PROPOSAL: &str = "proposal.md";
// Today's refusal when a previously shared repository is missing at its registered URL.
const MISSING: &str = "restic bot repository operation failed (exit 10: repository does not exist)";

#[derive(Debug, Deserialize, PartialEq)]
struct History {
    version: u32,
    repository_urls: BTreeSet<String>,
    password_refs: BTreeSet<String>,
    password_envs: BTreeSet<String>,
    repository_ids: BTreeSet<String>,
}

fn repository_id(fixture: &Fixture, bot: &Bot, repository: &str) -> String {
    let output = fixture.restic(bot, repository, &["cat", "config"]);
    success(&output);
    let config: Value = serde_json::from_slice(&output.stdout).unwrap();
    config["id"].as_str().unwrap().to_owned()
}

// Reads the accepted head snapshot itself, not a local import of it.
fn head_file(fixture: &Fixture, bot: &Bot, name: &str) -> Vec<u8> {
    let snapshot = fixture.current_shared_id(bot);
    let output = fixture.restic(bot, "shared", &["dump", &snapshot, &format!("/{name}")]);
    success(&output);
    output.stdout
}

fn head_history(fixture: &Fixture, bot: &Bot) -> History {
    serde_json::from_slice(&head_file(fixture, bot, "registry-history.json")).unwrap()
}

// A full move: a byte-identical copy at a new absolute path, and the old directory is gone, like a
// Drive generation restored on another host.
fn move_repository(root: &Path, bot: &mut Bot, repository: &str) {
    let old = bot.repository(repository).to_owned();
    let target = root.join("relocated").join(repository);
    copy_tree(Path::new(&old), &target);
    fs::remove_dir_all(old).unwrap();
    bot.config["repositories"][repository]["url"] = json!(target);
    bot.save();
}

fn author_from(source: &Path, bot: &Bot) {
    fs::remove_dir_all(bot.path("shared")).unwrap();
    copy_tree(source, &bot.path("shared"));
    fs::remove_file(bot.path("shared").join("registry-history.json")).unwrap();
}

// The copies keep their restic IDs and password names, so the history only gains their URLs.
fn assert_published_after_move(fixture: &Fixture, bot: &Bot, revision: u64, mut history: History) {
    let revision = revision + 1;
    assert_eq!(fixture.shared_head(bot)["revision"], revision);
    let record: Value = serde_json::from_slice(&head_file(fixture, bot, REVISION)).unwrap();
    assert_eq!(
        record,
        json!({
            "version": 3,
            "hub": "default",
            "repository_id": repository_id(fixture, bot, "shared"),
            "revision": revision,
        })
    );
    history
        .repository_urls
        .extend(["shared", "bot"].map(|name| bot.repository(name).to_owned()));
    assert_eq!(head_history(fixture, bot), history);
}

fn refused(fixture: &Fixture, bot: &Bot) {
    let edits = || {
        ["registry.yaml", REVISION, PROPOSAL]
            .map(|name| fs::read(bot.path("shared").join(name)).ok())
    };
    let (head, before) = (fixture.shared_head(bot), edits());
    let output = fixture.cli(bot, &["registry", "publish"]);
    failure(&output);
    assert_eq!(output.status.code(), Some(10), "{}", text(&output));
    assert!(text(&output).contains(MISSING), "{}", text(&output));
    assert_eq!(fixture.shared_head(bot), head);
    assert_eq!(edits(), before);
}

#[test]
fn fully_moved_repositories_accept_a_takeover_and_serve_a_peer() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let revision = fixture.shared_head(alpha)["revision"].as_u64().unwrap();
    let history = head_history(&fixture, alpha);
    let mut successor = fixture.bot("alpha", "alpha-2");
    move_repository(fixture.temp.path(), &mut successor, "shared");
    move_repository(fixture.temp.path(), &mut successor, "bot");
    author_from(&fixture.active_shared(alpha), &successor);
    fixture.registry(&successor, &[&successor]);
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    assert_published_after_move(&fixture, &successor, revision, history);
    let mut peer = fixture.bot("beta", "beta-1");
    peer.config["repositories"]["shared"]["url"] =
        successor.config["repositories"]["shared"]["url"].clone();
    peer.save();
    fixture.ok(&peer, &["setup"]);
    fixture.ok(&peer, &["sync"]);
    assert!(
        fixture
            .active_shared(&peer)
            .ends_with(fixture.current_shared_id(&peer))
    );
}

#[test]
fn fully_moved_bot_publishes_its_new_path_and_syncs_again() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let revision = fixture.shared_head(&fixture.alpha)["revision"]
        .as_u64()
        .unwrap();
    let history = head_history(&fixture, &fixture.alpha);
    move_repository(fixture.temp.path(), &mut fixture.alpha, "shared");
    move_repository(fixture.temp.path(), &mut fixture.alpha, "bot");
    let alpha = &fixture.alpha;
    // The accepted registry still names the old Bot path until this Bot publishes the new one.
    let output = fixture.cli(alpha, &["sync"]);
    failure(&output);
    assert!(
        text(&output)
            .contains("Bot repository and sharing configuration must match the current registry"),
        "{}",
        text(&output)
    );
    fixture.author_current(alpha);
    fixture.registry(alpha, &[alpha]);
    fixture.ok(alpha, &["registry", "publish"]);
    assert_published_after_move(&fixture, alpha, revision, history);
    fixture.ok(alpha, &["sync"]);
}

#[test]
fn a_different_repository_at_the_new_path_cannot_stand_in_for_a_deleted_one() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::remove_dir_all(fixture.alpha.repository("bot")).unwrap();
    fixture.alpha.config["repositories"]["bot"]["url"] =
        json!(fixture.temp.path().join("relocated/bot"));
    fixture.alpha.save();
    let alpha = &fixture.alpha;
    // It opens with the same password, but its fresh restic ID was never shared.
    success(&fixture.restic(alpha, "bot", &["init"]));
    fixture.author_current(alpha);
    fixture.registry(alpha, &[alpha]);
    write(alpha.path("shared").join(PROPOSAL), "unpublished proposal");
    refused(&fixture, alpha);
}

#[test]
fn a_deleted_shared_repository_without_a_moved_copy_still_refuses_publication() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let beta = fixture.bot("beta", "beta-1");
    fixture.ok(&beta, &["setup"]);
    fixture.ok(alpha, &["sync"]);
    fixture.author_current(alpha);
    fixture.registry(alpha, &[alpha]);
    write(alpha.path("shared").join(PROPOSAL), "unpublished proposal");
    fs::remove_dir_all(beta.repository("bot")).unwrap();
    refused(&fixture, alpha);
}
