#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const REVISION: &str = "shared-revision.json";
const DECISIONS: &str = "decisions.yaml";
const PROPOSAL: &str = "proposal.md";
const AUTHORING_BASE: &str = "Missing or invalid authoring revision base";
const DECISION_SCOPE: &str = "Decision hub/repository scope does not match configuration";

fn digest(url: &str) -> String {
    format!("{:x}", Sha256::digest(url.as_bytes()))
}

// The exact bytes 0.1.x wrote: the record is bound to the URL string, not to the repository.
fn legacy_revision(url: &str, revision: u64) -> String {
    format!(
        r#"{{"version":2,"hub":"default","repository_sha256":"{}","revision":{revision}}}"#,
        digest(url)
    )
}

// A hub-wide decision record; `scope` holds its version and repository binding.
fn decisions(scope: &str) -> String {
    format!(
        "{scope}\nhub: default\ncaveman:\n  enabled: true\n  basis: user_decision\n  source: \"Synthetic relocation decision\"\n  recorded_by: synthetic-user\n  recorded_at: \"2026-11-01T01:02:03Z\"\n  original_decision_at: null\n"
    )
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

fn assert_version_3(fixture: &Fixture, bot: &Bot) {
    let record: Value = serde_json::from_slice(&head_file(fixture, bot, REVISION)).unwrap();
    assert_eq!(
        record,
        json!({
            "version": 3,
            "hub": "default",
            "repository_id": repository_id(fixture, bot, "shared"),
            "revision": fixture.shared_head(bot)["revision"],
        })
    );
}

// A byte-identical copy at a new absolute path, like a Drive generation restored on another host.
fn relocate(root: &Path, bot: &mut Bot, repository: &str) {
    let target = root.join("relocated").join(repository);
    copy_tree(Path::new(bot.repository(repository)), &target);
    bot.config["repositories"][repository]["url"] = json!(target);
    bot.save();
}

fn author_from(source: &Path, bot: &Bot) {
    fs::remove_dir_all(bot.path("shared")).unwrap();
    copy_tree(source, &bot.path("shared"));
    fs::remove_file(bot.path("shared").join("registry-history.json")).unwrap();
}

// Accepts the current tree as the next head the way 0.1.x published it: its revision and decision
// records are bound to today's URL string. The snapshot keeps the head snapshot's native identity.
fn accept_legacy(fixture: &Fixture, bot: &Bot) -> PathBuf {
    let url = bot.repository("shared");
    let mut head = fixture.shared_head(bot);
    let revision = head["revision"].as_u64().unwrap() + 1;
    let tree = fixture.temp.path().join("legacy-head");
    copy_tree(&fixture.active_shared(bot), &tree);
    write(tree.join(REVISION), legacy_revision(url, revision));
    write(
        tree.join(DECISIONS),
        decisions(&format!(
            "version: 1\nrepository_sha256: \"{}\"",
            digest(url)
        )),
    );
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
        Path::new(url).join("uah-head.json"),
        serde_json::to_vec(&head).unwrap(),
    )
    .unwrap();
    tree
}

fn refused(fixture: &Fixture, bot: &Bot, reason: &str) {
    let edits =
        || [REVISION, DECISIONS, PROPOSAL].map(|name| fs::read(bot.path("shared").join(name)).ok());
    let (head, before) = (fixture.shared_head(bot), edits());
    let output = fixture.cli(bot, &["registry", "publish"]);
    failure(&output);
    assert!(
        text(&output).contains(reason),
        "expected {reason}: {}",
        text(&output)
    );
    assert_eq!(fixture.shared_head(bot), head);
    assert_eq!(edits(), before);
}

#[test]
fn relocated_repositories_accept_a_takeover_and_a_peer_by_restic_repository_id() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let mut successor = fixture.bot("alpha", "alpha-2");
    relocate(fixture.temp.path(), &mut successor, "shared");
    relocate(fixture.temp.path(), &mut successor, "bot");
    author_from(&fixture.active_shared(&fixture.alpha), &successor);
    fixture.registry(&successor, &[&successor]);
    fixture.ok(&successor, &["registry", "publish", "--takeover"]);
    assert_version_3(&fixture, &successor);
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
fn legacy_records_at_an_unchanged_url_are_accepted_and_the_next_revision_is_version_3() {
    let fixture = Fixture::new();
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["setup"]);
    let legacy = accept_legacy(&fixture, alpha);
    fixture.ok(alpha, &["sync"]);
    fixture.author_current(alpha);
    fixture.ok(alpha, &["registry", "publish"]);
    assert_version_3(&fixture, alpha);
    assert_eq!(
        head_file(&fixture, alpha, DECISIONS),
        fs::read(legacy.join(DECISIONS)).unwrap()
    );
}

#[test]
fn relocated_host_imports_legacy_records_and_publishes_from_a_byte_identical_base() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let legacy = accept_legacy(&fixture, &fixture.alpha);
    relocate(fixture.temp.path(), &mut fixture.alpha, "shared");
    let alpha = &fixture.alpha;
    fixture.ok(alpha, &["sync"]);
    fixture.author_current(alpha);
    fixture.ok(alpha, &["registry", "publish"]);
    assert_version_3(&fixture, alpha);
    assert_eq!(
        head_file(&fixture, alpha, DECISIONS),
        fs::read(legacy.join(DECISIONS)).unwrap()
    );
}

#[test]
fn bases_bound_to_another_path_or_repository_are_refused_keeping_head_and_edits() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let legacy = accept_legacy(&fixture, &fixture.alpha);
    relocate(fixture.temp.path(), &mut fixture.alpha, "shared");
    let alpha = &fixture.alpha;
    author_from(&legacy, alpha);
    write(alpha.path("shared").join(PROPOSAL), b"unpublished proposal");
    let revision = fixture.shared_head(alpha)["revision"].clone();
    let base = alpha.path("shared").join(REVISION);
    // Bound to neither the configured URL (a) nor byte-identical to the head's record (b).
    write(
        &base,
        legacy_revision("/synthetic/elsewhere/shared", revision.as_u64().unwrap()),
    );
    refused(&fixture, alpha, AUTHORING_BASE);
    let v3 = |id: &str| {
        json!({"version": 3, "hub": "default", "repository_id": id, "revision": revision})
            .to_string()
    };
    let wrong = repository_id(&fixture, alpha, "bot");
    write(&base, v3(&wrong));
    refused(&fixture, alpha, AUTHORING_BASE);
    write(&base, v3(&repository_id(&fixture, alpha, "shared")));
    write(
        alpha.path("shared").join(DECISIONS),
        decisions(&format!("version: 2\nrepository_id: \"{wrong}\"")),
    );
    refused(&fixture, alpha, DECISION_SCOPE);
}

#[test]
fn version_2_decisions_bound_to_the_repository_id_are_accepted_after_relocation() {
    let mut fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.author_current(&fixture.alpha);
    let id = repository_id(&fixture, &fixture.alpha, "shared");
    let decided = decisions(&format!("version: 2\nrepository_id: \"{id}\""));
    write(fixture.alpha.path("shared").join(DECISIONS), &decided);
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    relocate(fixture.temp.path(), &mut fixture.alpha, "shared");
    // Publication refreshed the base; this one imports and checks both records at the new path.
    fixture.ok(&fixture.alpha, &["registry", "publish"]);
    assert_eq!(
        head_file(&fixture, &fixture.alpha, DECISIONS),
        decided.as_bytes()
    );
}
