use super::{REVISION_FILE, Revision};
use crate::config::Config;
use crate::decisions::Source;
use serde_json::json;
use std::fs;
use std::path::Path;

// printf '/srv/hub/shared' | sha256sum
const URL_DIGEST: &str = "8e9512f5826a13477b15fb81aac7005b74cb6ad15d29b9f05e2c431b75533a10";
const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

fn config() -> Config {
    let mut config = Config {
        hub: "default".into(),
        ..Config::default()
    };
    config.repositories.shared.url = Some("/srv/hub/shared".into());
    config
}

fn load(root: &Path, bytes: &str, source: Source<'_>) -> Option<u64> {
    fs::write(root.join(REVISION_FILE), bytes).unwrap();
    match Revision::load(root, &config(), source) {
        Ok(revision) => Some(revision.revision),
        Err(error) => {
            assert!(
                error
                    .message
                    .starts_with("Missing or invalid authoring revision base;"),
                "{}",
                error.message
            );
            None
        }
    }
}

#[test]
fn revision_is_written_as_version_3_with_the_repository_id() {
    let bytes = serde_json::to_string(&Revision::new(&config(), ID, 7)).unwrap();
    assert_eq!(
        bytes,
        format!(r#"{{"version":3,"hub":"default","repository_id":"{ID}","revision":7}}"#)
    );
}

#[test]
fn revision_base_scope_depends_on_its_source() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let record = |version: u32, field: &str, value: &str| {
        format!(r#"{{"version":{version},"hub":"default","{field}":"{value}","revision":7}}"#)
    };
    let current = record(3, "repository_id", ID);
    let unmoved = record(2, "repository_sha256", URL_DIGEST);
    let moved = record(2, "repository_sha256", OTHER);
    let local = |id, head| Source::Local { id, head };
    // Current records match the configured repository ID.
    assert_eq!(load(root, &current, local(Some(ID), None)), Some(7));
    assert_eq!(load(root, &current, local(Some(OTHER), None)), None);
    // Legacy records match an unmoved URL, or the accepted head's exact bytes.
    assert_eq!(load(root, &unmoved, local(Some(ID), None)), Some(7));
    assert_eq!(load(root, &moved, local(Some(ID), None)), None);
    let head = moved.as_bytes();
    assert_eq!(load(root, &moved, local(Some(ID), Some(head))), Some(7));
    let reordered =
        format!(r#"{{"revision":7,"version":2,"hub":"default","repository_sha256":"{OTHER}"}}"#);
    assert_eq!(load(root, &reordered, local(Some(ID), Some(head))), None);
    // Imported records are scoped by provenance: any well-formed scope of this hub.
    for valid in [&current, &moved, &record(3, "repository_id", OTHER)] {
        assert_eq!(load(root, valid, Source::Imported), Some(7));
    }
    for invalid in [
        record(2, "repository_id", ID),
        record(3, "repository_sha256", URL_DIGEST),
        record(4, "repository_id", ID),
        record(3, "repository_id", &ID.to_uppercase()),
        current.replace("\"default\"", "\"other\""),
        current.replace(":7}", ":0}"),
        current.replace('}', r#","extra":1}"#),
        json!({"version": 3, "hub": "default", "repository_id": ID, "repository_sha256": URL_DIGEST, "revision": 7}).to_string(),
    ] {
        assert_eq!(load(root, &invalid, Source::Imported), None, "{invalid}");
        let head = Source::Local {
            id: Some(ID),
            head: Some(invalid.as_bytes()),
        };
        assert_eq!(load(root, &invalid, head), None, "{invalid}");
    }
}
