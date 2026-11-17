use crate::config::Config;
use crate::decisions::{Decisions, FILE, Scope, Source, bundled_default, supplement};
use crate::selection::Export;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

const APPROVED_DIGEST: &str = "85db6f9972ed7cb33b710b049673f3bf7d86c8b4d122668b66dd9aa34c9f9b81";
const URL: &str = "/srv/hub/shared";
// printf '/srv/hub/shared' | sha256sum
const URL_DIGEST: &str = "8e9512f5826a13477b15fb81aac7005b74cb6ad15d29b9f05e2c431b75533a10";
const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_ID: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const SCOPE_MISMATCH: &str = "Decision hub/repository scope does not match configuration";

fn config(shared: &Path) -> Config {
    let mut config = Config {
        hub: "default".into(),
        ..Config::default()
    };
    config.repositories.shared.url = Some(URL.into());
    config.paths.shared = Some(shared.to_owned());
    config
}

fn record(version: u32, scope: &str) -> String {
    format!(
        "version: {version}\nhub: default\n{scope}caveman:\n  enabled: true\n  basis: user_decision\n  source: synthetic\n  recorded_by: synthetic-user\n  recorded_at: \"2026-11-01T01:02:03Z\"\n  original_decision_at: null\n"
    )
}

fn local(id: Option<&str>) -> Source<'_> {
    Source::Local { id, head: None }
}

fn load(root: &Path, source: Source<'_>) -> Result<Option<Decisions>, String> {
    Decisions::load(root, &config(root), source).map_err(|error| error.message)
}

#[test]
fn url_digest_fixture_matches_the_legacy_scope_rule() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        crate::decisions::repository_sha256(&config(root.path())),
        URL_DIGEST
    );
}

#[test]
fn bundled_default_preserves_the_confirmed_scoped_decision_and_provenance() {
    // The bundled record is legacy: any repository ID leaves its digest match intact.
    let record = match bundled_default("default", APPROVED_DIGEST, ID) {
        Ok(Some(record)) => record,
        Ok(None) => panic!("the exact approved scope must have its bundled decision"),
        Err(_) => panic!("the approved bundled record must be valid"),
    };
    let mut actual = serde_json::to_value(record).expect("typed decisions must serialize");
    // The optional original timestamp may serialize as null or be omitted.
    let original = actual["caveman"]
        .as_object_mut()
        .unwrap()
        .remove("original_decision_at");
    assert!(original.is_none_or(|value| value == Value::Null));
    assert_eq!(
        actual,
        json!({
            "version": 1,
            "hub": "default",
            "repository_sha256": APPROVED_DIGEST,
            "caveman": {
                "enabled": true,
                "basis": "user_decision",
                "source": "Kunimaru: Q4=A; default hub Caveman=true, relayed in this ownerless-hub session by Shotoku Taishi / Hashida Itaru",
                "recorded_by": "uah-own-orch",
                "recorded_at": "2026-11-04T08:15:37Z"
            }
        })
    );
}

#[test]
fn bundled_default_never_supplies_consent_for_another_hub_or_digest() {
    let wrong_digest = "0".repeat(64);
    let uppercase_digest = APPROVED_DIGEST.to_uppercase();
    for (hub, digest, id) in [
        ("default", wrong_digest.as_str(), ID),
        ("default", uppercase_digest.as_str(), ID),
        ("Default", APPROVED_DIGEST, ID),
        ("default-other", APPROVED_DIGEST, ID),
        ("muse", APPROVED_DIGEST, ID),
        // A legacy record is never matched by repository ID.
        ("default", wrong_digest.as_str(), APPROVED_DIGEST),
    ] {
        match bundled_default(hub, digest, id) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("bundled consent escaped its exact scope: {hub}/{digest}"),
            Err(_) => panic!("scope mismatch must return None: {hub}/{digest}"),
        }
    }
}

#[test]
fn each_record_version_carries_its_own_repository_field() {
    let id = format!("repository_id: {ID}\n");
    let digest = format!("repository_sha256: {URL_DIGEST}\n");
    for (version, scope) in [(1, &digest), (2, &id)] {
        assert!(Decisions::parse(record(version, scope).as_bytes()).is_ok());
    }
    let both = format!("{id}{digest}");
    let upper = format!("repository_id: {}\n", ID.to_uppercase());
    let short = format!("repository_id: {}\n", &ID[..63]);
    for (version, scope) in [
        (1, id.as_str()),
        (2, digest.as_str()),
        (3, id.as_str()),
        (0, digest.as_str()),
        (2, both.as_str()),
        (2, ""),
        (2, upper.as_str()),
        (2, short.as_str()),
    ] {
        match Decisions::parse(record(version, scope).as_bytes()) {
            Err(error) => assert_eq!(
                error.message, "Invalid decisions.yaml version or scope",
                "{version} {scope}"
            ),
            Ok(_) => panic!("accepted version {version} with {scope:?}"),
        }
    }
}

#[test]
fn record_sources_decide_how_the_repository_scope_matches() {
    let accepts =
        |source: Source<'_>, scope, bytes: &[u8]| source.accepts(scope, URL_DIGEST, bytes);
    let head = b"accepted head file".as_slice();
    let relocated = Source::Local {
        id: Some(ID),
        head: Some(head),
    };
    // Provenance already scopes imported records; only their hub is checked.
    assert!(accepts(Source::Imported, Scope::Id(OTHER_ID), b""));
    assert!(accepts(Source::Imported, Scope::Legacy(OTHER_ID), b""));
    // Current records match the configured repository ID exactly.
    assert!(accepts(local(Some(ID)), Scope::Id(ID), b""));
    assert!(!accepts(local(Some(OTHER_ID)), Scope::Id(ID), b""));
    assert!(!accepts(local(None), Scope::Id(ID), b""));
    // Legacy records match an unmoved URL, or the accepted head's exact file.
    assert!(accepts(local(None), Scope::Legacy(URL_DIGEST), b""));
    assert!(accepts(relocated, Scope::Legacy(OTHER_ID), head));
    assert!(!accepts(relocated, Scope::Legacy(OTHER_ID), b"edited"));
    assert!(!accepts(local(Some(ID)), Scope::Legacy(OTHER_ID), head));
    assert!(!accepts(
        local(Some(OTHER_ID)),
        Scope::Legacy(OTHER_ID),
        b""
    ));
}

#[test]
fn local_version_2_record_needs_the_existing_repository_id() {
    let root = tempfile::tempdir().unwrap();
    let write = |bytes: String| fs::write(root.path().join(FILE), bytes).unwrap();
    let current = record(2, &format!("repository_id: {ID}\n"));
    write(current.clone());
    assert!(load(root.path(), local(Some(ID))).is_ok());
    assert_eq!(
        load(root.path(), local(Some(OTHER_ID))).err().as_deref(),
        Some(SCOPE_MISMATCH)
    );
    let missing = load(root.path(), local(None)).err().unwrap_or_default();
    assert!(
        missing.starts_with("decisions.yaml version 2 requires an existing shared repository"),
        "{missing}"
    );
    // A new hub can still use the legacy URL scope before its repository exists.
    write(record(1, &format!("repository_sha256: {URL_DIGEST}\n")));
    assert!(load(root.path(), local(None)).is_ok());
    // A wrong hub keeps its existing message even before the repository exists.
    write(current.replace("hub: default", "hub: other"));
    assert_eq!(
        load(root.path(), local(None)).err().as_deref(),
        Some(SCOPE_MISMATCH)
    );
}

#[test]
fn migration_supplement_writes_a_merged_record_in_the_current_format() {
    let shared = tempfile::tempdir().unwrap();
    let config = config(shared.path());
    let legacy = record(1, &format!("repository_sha256: {URL_DIGEST}\n"));
    fs::write(shared.path().join(FILE), &legacy).unwrap();
    let supplemented = |remote: Option<&str>| {
        let mut export = Export {
            tree: tempfile::tempdir().unwrap(),
            bytes: remote.map_or(0, |remote| remote.len() as u64),
        };
        if let Some(remote) = remote {
            fs::write(export.tree.path().join(FILE), remote).unwrap();
        }
        supplement(&mut export, &config, ID).unwrap_or_else(|error| panic!("{}", error.message));
        export
    };
    // An absent remote record adopts the explicit local bytes unchanged.
    let adopted = supplemented(None);
    assert_eq!(
        fs::read(adopted.tree.path().join(FILE)).unwrap(),
        legacy.as_bytes()
    );
    // A remote envelope without a record is merged and encoded by the CLI.
    let envelope = format!("version: 1\nhub: default\nrepository_sha256: {URL_DIGEST}\n");
    let merged = supplemented(Some(&envelope));
    let written: Value =
        serde_saphyr::from_slice(&fs::read(merged.tree.path().join(FILE)).unwrap()).unwrap();
    let mut expected: Value = serde_saphyr::from_str(&legacy).unwrap();
    expected["version"] = json!(2);
    expected
        .as_object_mut()
        .unwrap()
        .remove("repository_sha256");
    expected["repository_id"] = json!(ID);
    assert_eq!(written, expected);
    assert!(load(merged.tree.path(), local(Some(ID))).is_ok());
}
