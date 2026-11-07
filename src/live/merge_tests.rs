//! Tests for live merge.

use super::{Action, Base, conflict_name, plan, request_name};
use std::collections::BTreeMap;

const SHA: &str = "1a2b3c4d98fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn hashes(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, sha)| (path.to_string(), sha.to_string()))
        .collect()
}

fn bases(entries: &[(&str, &str, &str)]) -> BTreeMap<String, Base> {
    entries
        .iter()
        .map(|(path, object, plain)| {
            (
                path.to_string(),
                Base {
                    object: object.to_string(),
                    plain: plain.to_string(),
                },
            )
        })
        .collect()
}

#[test]
fn plan_follows_truth_table() {
    let path = "tasks/fix.md";
    // Object and plain differ, so comparing either side against the wrong base field fails.
    let applied = bases(&[(path, "base-object", "base-plain")]);
    let empty = BTreeMap::new();
    let at = |sha: Option<&str>| sha.map(|sha| hashes(&[(path, sha)])).unwrap_or_default();
    let push = || vec![Action::Push(path.to_string())];
    let pull = || {
        vec![Action::Pull {
            path: path.to_string(),
            object: "new-object".to_string(),
        }]
    };
    let conflict = |remote: Option<&str>| {
        vec![Action::Conflict {
            path: path.to_string(),
            remote: remote.map(str::to_string),
        }]
    };
    // Columns follow the brief: b = base, r = remote, l = local.
    for (row, base, remote, local, expected) in [
        ("∅ ∅ some", &empty, None, Some("new-plain"), push()),
        ("∅ some ∅", &empty, Some("new-object"), None, pull()),
        (
            "∅ some some",
            &empty,
            Some("new-object"),
            Some("new-plain"),
            conflict(Some("new-object")),
        ),
        (
            "some = =",
            &applied,
            Some("base-object"),
            Some("base-plain"),
            vec![],
        ),
        (
            "some = ≠",
            &applied,
            Some("base-object"),
            Some("new-plain"),
            push(),
        ),
        (
            "some = ∅",
            &applied,
            Some("base-object"),
            None,
            vec![Action::Delete(path.to_string())],
        ),
        (
            "some ≠ =",
            &applied,
            Some("new-object"),
            Some("base-plain"),
            pull(),
        ),
        (
            "some ∅ =",
            &applied,
            None,
            Some("base-plain"),
            vec![Action::Remove(path.to_string())],
        ),
        (
            "some ∅ ∅",
            &applied,
            None,
            None,
            vec![Action::Forget(path.to_string())],
        ),
        ("some ≠ ∅", &applied, Some("new-object"), None, pull()),
        (
            "some ∅ ≠",
            &applied,
            None,
            Some("new-plain"),
            conflict(None),
        ),
        (
            "some ≠ ≠",
            &applied,
            Some("new-object"),
            Some("new-plain"),
            conflict(Some("new-object")),
        ),
    ] {
        assert_eq!(
            plan(base, &at(remote), &at(local)),
            expected,
            "b r l = {row}"
        );
    }
}

#[test]
fn plan_is_sorted_and_skips_unchanged() {
    // Paths interleave across the three maps; b.md and f.md are unchanged.
    let base = bases(&[
        ("b.md", "b-object", "b-plain"),
        ("d.md", "d-object", "d-plain"),
        ("f.md", "f-object", "f-plain"),
    ]);
    let remote = hashes(&[
        ("b.md", "b-object"),
        ("c.md", "c-object"),
        ("d.md", "d-object-2"),
        ("f.md", "f-object"),
    ]);
    let local = hashes(&[
        ("a.md", "a-plain"),
        ("b.md", "b-plain"),
        ("d.md", "d-plain"),
        ("e.md", "e-plain"),
        ("f.md", "f-plain"),
    ]);
    assert_eq!(
        plan(&base, &remote, &local),
        vec![
            Action::Push("a.md".to_string()),
            Action::Pull {
                path: "c.md".to_string(),
                object: "c-object".to_string(),
            },
            Action::Pull {
                path: "d.md".to_string(),
                object: "d-object-2".to_string(),
            },
            Action::Push("e.md".to_string()),
        ]
    );
}

#[test]
fn conflict_name_examples() {
    for (path, expected) in [
        ("tasks/fix.md", "tasks/fix.conflict-1a2b3c4d.md"),
        ("RULES", "RULES.conflict-1a2b3c4d"),
        (".hidden", ".hidden.conflict-1a2b3c4d"),
        ("a/b.tar.gz", "a/b.tar.conflict-1a2b3c4d.gz"),
        // Dots outside the final component never count.
        ("notes.d/RULES", "notes.d/RULES.conflict-1a2b3c4d"),
        ("notes/.hidden", "notes/.hidden.conflict-1a2b3c4d"),
    ] {
        assert_eq!(conflict_name(path, SHA), expected, "{path}");
    }
}

#[test]
fn request_name_examples() {
    for (root, requester, source, expected) in [
        ("tasks/", "beta", "/x/req.md", "tasks/beta-1a2b3c4d.md"),
        ("tasks/", "beta", "notes", "tasks/beta-1a2b3c4d"),
        // The extension follows the conflict_name rule.
        ("tasks/", "beta", "/x/a.tar.gz", "tasks/beta-1a2b3c4d.gz"),
        ("tasks/", "beta", "/x/.hidden", "tasks/beta-1a2b3c4d"),
        ("tasks/", "beta", "/x.d/notes", "tasks/beta-1a2b3c4d"),
        (
            "inbox/alpha/",
            "gamma",
            "plan.txt",
            "inbox/alpha/gamma-1a2b3c4d.txt",
        ),
    ] {
        assert_eq!(
            request_name(root, requester, SHA, source),
            expected,
            "{root} {requester} {source}"
        );
    }
}
