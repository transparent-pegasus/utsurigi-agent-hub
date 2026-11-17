//! Tests for the identifier history and for publishing a registry whose local
//! repositories moved.

#![cfg(unix)]

use super::{History, Missing};
use crate::config::{Config, Repository};
use crate::error::Result;
use crate::registry::{Entry, Registry};
use crate::restic::Restic;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// Answers `cat config` with the `id` file of a repository directory and
// `snapshots` with its `snapshots` file. Like restic, it exits 10 when the
// repository does not exist.
const RESTIC: &str = r#"#!/bin/sh
[ "$1" = version ] && { echo restic 0.19.1; exit 0; }
[ "$1" = --repo ] || exit 2
[ -f "$2/id" ] || exit 10
case " $* " in *" snapshots "*) exec cat "$2/snapshots" ;; esac
read -r id < "$2/id"
printf '{"id":"%s"}\n' "$id"
"#;
const SHARED: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const ALPHA: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const FRESH: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const BETA: &str = "4444444444444444444444444444444444444444444444444444444444444444";
// The fake ignores passwords. `cargo test` sets both variables, so no test
// changes the environment.
const SHARED_PASSWORD_ENV: &str = "PATH";
const PRIVATE_PASSWORD_ENV: &str = "CARGO_MANIFEST_DIR";
const MISSING: &str = "restic bot repository operation failed (exit 10: repository does not exist)";

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("history operation failed: {}", error.message))
}

fn url(root: &Path, name: &str) -> String {
    root.join(name).to_str().unwrap().into()
}

// A snapshot of `bot` in `hub`, as `restic snapshots --json` lists it.
fn snapshot(bot: &str, hub: &str, kind: &str) -> Value {
    json!({
        "id": "5".repeat(64),
        "hostname": bot,
        "time": "2026-11-01T00:00:00Z",
        "tags": [
            "utsurigi-agent-hub",
            format!("hub:{hub}"),
            format!("bot:{bot}"),
            format!("kind:{kind}"),
        ],
    })
}

// `alpha` was deleted after its exact copy `alpha-moved`; `clone` holds the
// same master key at another path. `beta-copy` and `shared-copy` are copies of
// Bot beta's and of the hub repository. The last five each break one condition
// of attributing a repository to Bot alpha by its content.
fn repositories(root: &Path) {
    let restic = root.join("restic");
    fs::write(&restic, RESTIC).unwrap();
    fs::set_permissions(&restic, fs::Permissions::from_mode(0o700)).unwrap();
    let alpha = snapshot("alpha", "default", "bot");
    let hub = snapshot("alpha", "default", "shared-v2");
    for (name, id, snapshots) in [
        ("shared", SHARED, json!([hub])),
        ("alpha-moved", ALPHA, json!([alpha, alpha])),
        ("clone", ALPHA, json!([alpha])),
        ("fresh", FRESH, json!([alpha])),
        (
            "beta-copy",
            BETA,
            json!([snapshot("beta", "default", "bot")]),
        ),
        ("shared-copy", SHARED, json!([hub])),
        ("empty", ALPHA, json!([])),
        (
            "mixed",
            ALPHA,
            json!([alpha, snapshot("beta", "default", "bot")]),
        ),
        (
            "hub-kind",
            ALPHA,
            json!([alpha, snapshot("alpha", "default", "shared")]),
        ),
        (
            "other-hub",
            ALPHA,
            json!([snapshot("alpha", "other", "bot")]),
        ),
        ("shared-id", SHARED, json!([alpha])),
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("id"), id).unwrap();
        fs::write(root.join(name).join("snapshots"), snapshots.to_string()).unwrap();
    }
}

fn config(root: &Path, provider: &str) -> Config {
    let mut config = Config {
        hub: "default".into(),
        ..Config::default()
    };
    config.storage.provider = provider.into();
    config.encryption.executable = root.join("restic");
    config.paths.state = Some(root.join("state"));
    config.repositories.shared = Repository {
        url: Some(url(root, "shared")),
        password_ref: None,
        password_env: Some(SHARED_PASSWORD_ENV.into()),
    };
    config
}

fn entry(id: &str, repository: String, password_env: &str) -> Entry {
    Entry {
        id: id.into(),
        runtime_id: format!("{id}-runtime"),
        repository,
        password_ref: None,
        password_env: password_env.into(),
        shared: password_env == SHARED_PASSWORD_ENV,
        machines: Vec::new(),
    }
}

fn registry(bots: Vec<Entry>) -> Registry {
    Registry {
        version: 2,
        hub: "default".into(),
        max_bots: 50,
        bots,
    }
}

// Known IDs, and the IDs recorded per Bot; none, as loaded from version 1.
fn known(ids: &[&str], bots: &[(&str, &str)]) -> History {
    let mut history = History {
        repository_ids: ids.iter().map(|id| id.to_string()).collect(),
        ..History::new()
    };
    for (bot, id) in bots {
        history
            .bot_repository_ids
            .entry(bot.to_string())
            .or_default()
            .insert(id.to_string());
    }
    history
}

fn refused<T>(case: &str, result: Result<T>) {
    let error = result.err().unwrap_or_else(|| panic!("{case}: accepted"));
    assert_eq!(error.code, 10, "{case}: {}", error.message);
    assert!(
        error.message.starts_with(MISSING),
        "{case}: {}",
        error.message
    );
}

#[test]
fn missing_local_repository_counts_as_moved_only_to_a_known_id_of_the_same_bot() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    repositories(root);
    let (local, r2) = (config(root, "local"), config(root, "r2"));
    let restic = must(Restic::open(&local, false));
    let at = |id, name, password_env| registry(vec![entry(id, url(root, name), password_env)]);
    let previous = at("alpha", "alpha", SHARED_PASSWORD_ENV);
    let moved = at("alpha", "alpha-moved", SHARED_PASSWORD_ENV);
    let before = known(&[SHARED, ALPHA, BETA], &[("alpha", ALPHA), ("beta", BETA)]);
    let observe = |config: &Config, candidate: &Registry, known: &History| {
        let mut history = before.clone();
        history
            .observe(&previous, config, &restic, Missing::Moved(candidate, known))
            .map(|()| history)
    };

    let accepted = must(observe(&local, &moved, &before));
    // The old URL stays in the history, and the missing repository adds no ID.
    assert!(accepted.repository_urls.contains(&url(root, "alpha")));
    assert_eq!(accepted.repository_ids, before.repository_ids);
    assert_eq!(accepted.bot_repository_ids, before.bot_repository_ids);

    let fresh = at("alpha", "fresh", SHARED_PASSWORD_ENV);
    let undelivered = at("alpha", "alpha-moved", "UAH_HISTORY_TEST_UNDELIVERED");
    let other_bot = at("beta", "alpha-moved", SHARED_PASSWORD_ENV);
    let beta_copy = at("alpha", "beta-copy", SHARED_PASSWORD_ENV);
    let hub_copy = at("alpha", "shared-copy", SHARED_PASSWORD_ENV);
    // The observed history holds ALPHA, but only the earlier snapshot may vouch.
    let earlier = known(&[SHARED], &[]);
    // FRESH is known and holds alpha's snapshots, but alpha's IDs do not list it.
    let unlisted = known(&[SHARED, ALPHA, FRESH], &[("alpha", ALPHA)]);
    // Alpha's IDs list beta's or the hub's, as after alpha was pointed at a copy.
    let beta_listed = known(
        &[SHARED, ALPHA, BETA],
        &[("alpha", ALPHA), ("alpha", BETA), ("beta", BETA)],
    );
    let hub_listed = known(&[SHARED, ALPHA], &[("alpha", ALPHA), ("alpha", SHARED)]);
    for (case, config, candidate, known) in [
        ("unknown ID", &local, &fresh, &before),
        ("copy missing too", &local, &previous, &before),
        ("copy password undelivered", &local, &undelivered, &before),
        ("another Bot at the copy", &local, &other_bot, &before),
        ("ID known only after observing", &local, &moved, &earlier),
        (
            "copy of another Bot's repository",
            &local,
            &beta_copy,
            &before,
        ),
        ("copy of the hub repository", &local, &hub_copy, &before),
        (
            "known ID that the Bot's IDs do not list",
            &local,
            &fresh,
            &unlisted,
        ),
        (
            "copy of another Bot's repository listed for the Bot",
            &local,
            &beta_copy,
            &beta_listed,
        ),
        (
            "copy of the hub repository listed for the Bot",
            &local,
            &hub_copy,
            &hub_listed,
        ),
        // Same transport, r2 configuration: only the provider gate differs.
        ("r2", &r2, &moved, &before),
    ] {
        refused(case, observe(config, candidate, known));
    }
}

#[test]
fn a_bot_without_recorded_ids_moves_only_to_a_known_id_whose_snapshots_are_its_own() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    repositories(root);
    let local = config(root, "local");
    let restic = must(Restic::open(&local, false));
    let at = |name| registry(vec![entry("alpha", url(root, name), SHARED_PASSWORD_ENV)]);
    let previous = at("alpha");
    let observe = |candidate: &Registry, known: &History| {
        let mut history = known.clone();
        history
            .observe(&previous, &local, &restic, Missing::Moved(candidate, known))
            .map(|()| history)
    };
    let legacy = known(&[SHARED, ALPHA, BETA], &[]);

    let moved = at("alpha-moved");
    let mut accepted = must(observe(&moved, &legacy));
    // Observing the candidate records ALPHA under alpha; the hub's ID stays unlisted.
    must(accepted.observe(&moved, &local, &restic, Missing::Allowed));
    assert_eq!(
        accepted.bot_repository_ids,
        BTreeMap::from([("alpha".to_owned(), BTreeSet::from([ALPHA.to_owned()]))])
    );

    let beta_listed = known(&[SHARED, ALPHA, BETA], &[("beta", ALPHA)]);
    for (case, name, known) in [
        ("copy of another Bot's repository", "beta-copy", &legacy),
        ("copy of the hub repository", "shared-copy", &legacy),
        ("unknown ID", "fresh", &legacy),
        ("no snapshot", "empty", &legacy),
        ("one snapshot of another Bot", "mixed", &legacy),
        ("one hub snapshot", "hub-kind", &legacy),
        ("snapshots of another hub", "other-hub", &legacy),
        ("the shared repository's ID", "shared-id", &legacy),
        ("another Bot's ID", "alpha-moved", &beta_listed),
    ] {
        refused(case, observe(&at(name), known));
    }
}

#[test]
fn moved_repository_skips_the_pairwise_check_but_its_private_clone_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    repositories(root);
    let (local, r2) = (config(root, "local"), config(root, "r2"));
    let restic = must(Restic::open(&local, false));
    let before = known(&[SHARED, ALPHA], &[("alpha", ALPHA)]);
    let alpha_at = |name: &str| entry("alpha", url(root, name), SHARED_PASSWORD_ENV);
    let candidate = |private: &str| {
        registry(vec![
            alpha_at("alpha-moved"),
            entry("gamma", url(root, private), PRIVATE_PASSWORD_ENV),
        ])
    };
    // The identity checks of `publish`, in its order.
    let publish = |previous: &str, private: &str| -> Result<()> {
        let (previous, candidate) = (registry(vec![alpha_at(previous)]), candidate(private));
        let mut history = before.clone();
        candidate.transition_from(&previous, &local, &restic)?;
        history.observe(
            &previous,
            &local,
            &restic,
            Missing::Moved(&candidate, &before),
        )?;
        history.observe(&candidate, &local, &restic, Missing::Allowed)?;
        history.check(&candidate, &restic)
    };

    must(publish("alpha", "fresh"));
    let refusal = |result: Result<()>| result.expect_err("published").message;
    assert_eq!(
        refusal(publish("alpha", "clone")),
        "Private Bot data requires a separate repository and master key"
    );
    // An existing prior repository is still compared pair by pair.
    assert_eq!(
        refusal(publish("alpha-moved", "clone")),
        "Repository reuse does not revoke a shared master key"
    );
    // With r2, the pairwise check still fails on the missing prior repository.
    let previous = registry(vec![alpha_at("alpha")]);
    let r2_transition = candidate("fresh").transition_from(&previous, &r2, &restic);
    assert!(refusal(r2_transition).starts_with(MISSING));
}

#[test]
fn version_1_loads_as_version_2_and_per_bot_ids_must_be_known_lowercase_ids() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let load = |value: &Value| {
        fs::write(root.join("registry-history.json"), value.to_string()).unwrap();
        History::load(root, &registry(Vec::new()))
    };
    let written = |history: History| serde_json::to_value(history).unwrap();
    let current = json!({
        "version": 2,
        "repository_urls": ["/hub/alpha", "/hub/shared"],
        "password_refs": [],
        "password_envs": ["HUB_PASSWORD"],
        "repository_ids": [SHARED, ALPHA],
        "bot_repository_ids": {"alpha": [ALPHA]},
    });
    assert_eq!(written(must(load(&current))), current);
    let mut legacy = current.clone();
    legacy["version"] = json!(1);
    legacy.as_object_mut().unwrap().remove("bot_repository_ids");
    let mut upgraded = current.clone();
    upgraded["bot_repository_ids"] = json!({});
    assert_eq!(written(must(load(&legacy))), upgraded);

    let upper = "A".repeat(64);
    for (case, changes) in [
        ("version 3", json!({"version": 3})),
        ("version 1 with per-Bot IDs", json!({"version": 1})),
        (
            "a Bot that is no identity",
            json!({"bot_repository_ids": {"alpha/beta": [ALPHA]}}),
        ),
        (
            "an ID missing from the known IDs",
            json!({"bot_repository_ids": {"alpha": [FRESH]}}),
        ),
        (
            "an uppercase ID",
            json!({"repository_ids": [SHARED, upper], "bot_repository_ids": {"alpha": [upper]}}),
        ),
        ("an unknown field", json!({"bot_ids": {}})),
    ] {
        let mut invalid = current.clone();
        for (field, value) in changes.as_object().unwrap() {
            invalid[field] = value.clone();
        }
        let error = load(&invalid)
            .err()
            .unwrap_or_else(|| panic!("{case}: loaded"));
        assert_eq!(
            error.message, "Missing or invalid shared identifier history",
            "{case}"
        );
    }
}

#[test]
fn pending_master_bytes_are_what_recording_the_new_ids_adds() {
    let (hub, bot) = ("6".repeat(64), "7".repeat(64));
    let length = |history: &History| serde_json::to_vec(history).unwrap().len() as u64;
    for (case, history) in [
        ("empty", History::new()),
        (
            "another Bot listed",
            known(&[SHARED, BETA], &[("beta", BETA)]),
        ),
        (
            "the Bot listed",
            known(&[SHARED, ALPHA], &[("alpha", ALPHA)]),
        ),
    ] {
        for (pending_hub, pending_bot) in [(true, false), (false, true), (true, true)] {
            let mut recorded = history.clone();
            if pending_hub {
                recorded.repository_ids.insert(hub.clone());
            }
            if pending_bot {
                recorded.repository_ids.insert(bot.clone());
                recorded
                    .bot_repository_ids
                    .entry("alpha".into())
                    .or_default()
                    .insert(bot.clone());
            }
            let count = u64::from(pending_hub) + u64::from(pending_bot);
            assert_eq!(
                history.pending_master_bytes(count, pending_bot.then_some("alpha")),
                length(&recorded) - length(&history),
                "{case}, hub {pending_hub}, bot {pending_bot}"
            );
        }
    }
}
