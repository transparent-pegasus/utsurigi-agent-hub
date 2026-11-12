//! Tests for publishing a registry whose local repositories moved.

#![cfg(unix)]

use super::{History, Missing};
use crate::config::{Config, Repository};
use crate::error::Result;
use crate::registry::{Entry, Registry};
use crate::restic::Restic;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

// Answers `cat config` with the `id` file of a repository directory. Like restic,
// it exits 10 when the repository does not exist.
const RESTIC: &str = r#"#!/bin/sh
[ "$1" = version ] && { echo restic 0.19.1; exit 0; }
[ "$1" = --repo ] || exit 2
[ -f "$2/id" ] || exit 10
read -r id < "$2/id"
printf '{"id":"%s"}\n' "$id"
"#;
const SHARED: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const ALPHA: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const FRESH: &str = "3333333333333333333333333333333333333333333333333333333333333333";
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

// `alpha` was deleted after its exact copy `alpha-moved`; `clone` holds the
// same master key at another path.
fn repositories(root: &Path) {
    let restic = root.join("restic");
    fs::write(&restic, RESTIC).unwrap();
    fs::set_permissions(&restic, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, id) in [
        ("shared", SHARED),
        ("alpha-moved", ALPHA),
        ("clone", ALPHA),
        ("fresh", FRESH),
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("id"), id).unwrap();
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

fn known(ids: &[&str]) -> History {
    History {
        repository_ids: ids.iter().map(|id| id.to_string()).collect(),
        ..History::new()
    }
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
    let before = known(&[SHARED, ALPHA]);
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

    let fresh = at("alpha", "fresh", SHARED_PASSWORD_ENV);
    let undelivered = at("alpha", "alpha-moved", "UAH_HISTORY_TEST_UNDELIVERED");
    let other_bot = at("beta", "alpha-moved", SHARED_PASSWORD_ENV);
    // The observed history holds ALPHA, but only the earlier snapshot may vouch.
    let earlier = known(&[SHARED]);
    for (case, config, candidate, known) in [
        ("unknown ID", &local, &fresh, &before),
        ("copy missing too", &local, &previous, &before),
        ("copy password undelivered", &local, &undelivered, &before),
        ("another Bot at the copy", &local, &other_bot, &before),
        ("ID known only after observing", &local, &moved, &earlier),
        // Same transport, r2 configuration: only the provider gate differs.
        ("r2", &r2, &moved, &before),
    ] {
        let error = observe(config, candidate, known)
            .err()
            .unwrap_or_else(|| panic!("{case}: accepted"));
        assert_eq!(error.code, 10, "{case}: {}", error.message);
        assert!(
            error.message.starts_with(MISSING),
            "{case}: {}",
            error.message
        );
    }
}

#[test]
fn moved_repository_skips_the_pairwise_check_but_its_private_clone_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    repositories(root);
    let (local, r2) = (config(root, "local"), config(root, "r2"));
    let restic = must(Restic::open(&local, false));
    let before = known(&[SHARED, ALPHA]);
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
