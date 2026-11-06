//! Tests for the live list and hub index wire formats, written from the Task 2
//! brief without reading the implementation. Expected wire values come from the
//! brief, never from the product serializer.

use super::{
    Class, Entry, HubMark, Index, List, MAX_BYTES, MAX_ENTRIES, MAX_ROOTS, Root, Spec, bot_key,
    hex, relative_path, sha256_hex, unhex,
};
use crate::error::Result;
use crate::live::policy::Policy;
use serde_json::{Value, json};
use std::collections::BTreeMap;

const SALT: &str = "00112233445566778899aabbccddeeff";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const INVALID_LIST: &str = "Invalid live list";
const INVALID_INDEX: &str = "Invalid live index";
const INVALID_ROOTS: &str = "Invalid live roots";
const INVALID_PATH: &str = "Invalid live path";

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("live format failed: {}", error.message))
}

fn error<T>(result: Result<T>) -> Option<String> {
    result.err().map(|error| error.message)
}

fn bytes(wire: &Value) -> Vec<u8> {
    serde_json::to_vec(wire).expect("fixture JSON serializes")
}

/// Valid JSON, padded with insignificant whitespace to exactly `length` bytes.
fn padded(wire: &Value, length: usize) -> Vec<u8> {
    let mut document = bytes(wire);
    assert!(document.len() <= length, "fixture exceeds {length} bytes");
    document.resize(length, b' ');
    document
}

fn edited(mut wire: Value, pointer: &str, value: Value) -> Vec<u8> {
    *wire.pointer_mut(pointer).expect("fixture field") = value;
    bytes(&wire)
}

fn with_unknown_field(mut wire: Value, pointer: &str) -> Vec<u8> {
    wire.pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .expect("fixture object")
        .insert("extra".into(), json!(1));
    bytes(&wire)
}

fn list() -> List {
    List {
        version: 1,
        revision: 12,
        salt: SALT.into(),
        spec: "c0ffee".into(),
        entries: vec![
            Entry {
                name: "0a1b".into(),
                sha256: A.into(),
            },
            Entry {
                name: "2c3d4e".into(),
                sha256: B.into(),
            },
        ],
    }
}

fn list_wire() -> Value {
    json!({
        "version": 1,
        "revision": 12,
        "salt": SALT,
        "spec": "c0ffee",
        "entries": [{"name": "0a1b", "sha256": A}, {"name": "2c3d4e", "sha256": B}]
    })
}

/// `count` entries with distinct even-length names and distinct digests.
fn entries(count: usize) -> Value {
    (0..count)
        .map(|i| json!({"name": format!("{i:04x}"), "sha256": format!("{i:064x}")}))
        .collect()
}

fn index() -> Index {
    Index {
        version: 1,
        hub: HubMark {
            revision: 7,
            snapshot: A.into(),
        },
        bots: BTreeMap::from([(B.into(), 3), (C.into(), 1)]),
    }
}

fn index_wire() -> Value {
    json!({"version": 1, "hub": {"revision": 7, "snapshot": A}, "bots": {B: 3, C: 1}})
}

fn root(path: &str, class: Class) -> Root {
    Root {
        path: path.into(),
        class,
    }
}

fn spec(roots: Vec<Root>) -> Spec {
    Spec {
        roots,
        policy: Policy::default(),
    }
}

/// `count` distinct file roots; file roots may share a parent directory.
fn files(count: usize) -> Vec<Root> {
    (0..count)
        .map(|i| root(&format!("notes/{i}.md"), Class::Task))
        .collect()
}

#[test]
fn limits_match_the_brief() {
    assert_eq!((MAX_BYTES, MAX_ENTRIES, MAX_ROOTS), (1 << 20, 2000, 64));
}

#[test]
fn list_round_trips() {
    let encoded: Vec<u8> = must(list().encode());
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).expect("the live list is JSON"),
        list_wire()
    );
    assert_eq!(must(List::decode(&encoded)), list());
    assert_eq!(must(List::decode(&bytes(&list_wire()))), list());
    let empty = List {
        entries: Vec::new(),
        ..list()
    };
    assert_eq!(must(List::decode(&must(empty.encode()))), empty);
}

#[test]
fn list_decode_rejects_invalid() {
    let cases = [
        ("version 2", edited(list_wire(), "/version", json!(2))),
        ("version 0", edited(list_wire(), "/version", json!(0))),
        (
            "version as text",
            edited(list_wire(), "/version", json!("1")),
        ),
        ("revision 0", edited(list_wire(), "/revision", json!(0))),
        (
            "uppercase salt",
            edited(list_wire(), "/salt", json!(SALT.to_uppercase())),
        ),
        (
            "uppercase spec",
            edited(list_wire(), "/spec", json!("C0FFEE")),
        ),
        (
            "uppercase name",
            edited(list_wire(), "/entries/0/name", json!("0A1B")),
        ),
        (
            "uppercase sha256",
            edited(list_wire(), "/entries/0/sha256", json!(A.to_uppercase())),
        ),
        ("odd spec", edited(list_wire(), "/spec", json!("c0ffe"))),
        (
            "odd name",
            edited(list_wire(), "/entries/0/name", json!("0a1")),
        ),
        (
            "non-hex spec",
            edited(list_wire(), "/spec", json!("c0ffeg")),
        ),
        ("empty spec", edited(list_wire(), "/spec", json!(""))),
        (
            "empty name",
            edited(list_wire(), "/entries/0/name", json!("")),
        ),
        (
            "salt of 31 hex digits",
            edited(list_wire(), "/salt", json!(&SALT[..31])),
        ),
        (
            "salt of 15 bytes",
            edited(list_wire(), "/salt", json!(&SALT[..30])),
        ),
        (
            "salt of 17 bytes",
            edited(list_wire(), "/salt", json!(format!("{SALT}00"))),
        ),
        (
            "salt of 31 bytes",
            edited(
                list_wire(),
                "/salt",
                json!(format!("{SALT}{}", &SALT[..30])),
            ),
        ),
        (
            "short sha256",
            edited(list_wire(), "/entries/0/sha256", json!(&A[..63])),
        ),
        (
            "long sha256",
            edited(list_wire(), "/entries/0/sha256", json!(format!("{A}a"))),
        ),
        (
            "duplicate sha256",
            edited(list_wire(), "/entries/1/sha256", json!(A)),
        ),
        (
            "2001 entries",
            edited(list_wire(), "/entries", entries(MAX_ENTRIES + 1)),
        ),
        ("unknown field", with_unknown_field(list_wire(), "")),
        (
            "unknown entry field",
            with_unknown_field(list_wire(), "/entries/0"),
        ),
        (
            "over MAX_BYTES",
            padded(&list_wire(), MAX_BYTES as usize + 1),
        ),
        ("not JSON", b"not json".to_vec()),
    ];
    for (case, document) in cases {
        assert_eq!(
            error(List::decode(&document)).as_deref(),
            Some(INVALID_LIST),
            "{case}"
        );
    }
}

#[test]
fn list_decode_accepts_its_limits() {
    let mut wire = list_wire();
    wire["entries"] = entries(MAX_ENTRIES);
    assert_eq!(must(List::decode(&bytes(&wire))).entries.len(), MAX_ENTRIES);
    assert_eq!(
        must(List::decode(&padded(&list_wire(), MAX_BYTES as usize))),
        list()
    );
}

#[test]
fn list_encode_rejects_lists_over_the_size_limit() {
    let max = MAX_BYTES as usize;
    let fixed = |list: &List| must(list.encode()).len() - list.spec.len();
    let mut large = list();
    if (max - fixed(&large)) % 2 == 1 {
        large.revision *= 10; // one more digit leaves an even number of bytes for hex
    }
    large.spec = "ab".repeat((max - fixed(&large)) / 2);
    let encoded = must(large.encode());
    assert_eq!(encoded.len(), max);
    assert_eq!(must(List::decode(&encoded)), large);
    large.spec.push_str("ab");
    assert_eq!(
        error(large.encode()).as_deref(),
        Some("Live list exceeds its size limit")
    );
}

#[test]
fn index_round_trips() {
    let encoded: Vec<u8> = must(index().encode());
    assert_eq!(
        serde_json::from_slice::<Value>(&encoded).expect("the live index is JSON"),
        index_wire()
    );
    assert_eq!(must(Index::decode(&encoded)), index());
    assert_eq!(must(Index::decode(&bytes(&index_wire()))), index());
    let fresh = Index {
        bots: BTreeMap::new(),
        ..index()
    };
    assert_eq!(must(Index::decode(&must(fresh.encode()))), fresh);
}

#[test]
fn index_decode_rejects_invalid() {
    let bot = format!("/bots/{B}");
    let cases = [
        ("version 2", edited(index_wire(), "/version", json!(2))),
        (
            "hub revision 0",
            edited(index_wire(), "/hub/revision", json!(0)),
        ),
        (
            "short snapshot",
            edited(index_wire(), "/hub/snapshot", json!(&A[..63])),
        ),
        (
            "long snapshot",
            edited(index_wire(), "/hub/snapshot", json!(format!("{A}a"))),
        ),
        (
            "uppercase snapshot",
            edited(index_wire(), "/hub/snapshot", json!(A.to_uppercase())),
        ),
        (
            "uppercase key",
            edited(index_wire(), "/bots", json!({B.to_uppercase(): 3})),
        ),
        (
            "short key",
            edited(index_wire(), "/bots", json!({&B[..62]: 3})),
        ),
        ("value 0", edited(index_wire(), &bot, json!(0))),
        ("unknown field", with_unknown_field(index_wire(), "")),
        (
            "unknown hub field",
            with_unknown_field(index_wire(), "/hub"),
        ),
        (
            "over MAX_BYTES",
            padded(&index_wire(), MAX_BYTES as usize + 1),
        ),
        ("not JSON", b"not json".to_vec()),
    ];
    for (case, document) in cases {
        assert_eq!(
            error(Index::decode(&document)).as_deref(),
            Some(INVALID_INDEX),
            "{case}"
        );
    }
}

#[test]
fn index_decode_accepts_its_size_limit() {
    assert_eq!(
        must(Index::decode(&padded(&index_wire(), MAX_BYTES as usize))),
        index()
    );
}

#[test]
fn spec_wire_uses_lowercase_classes() {
    let typed = Spec {
        roots: vec![root("tasks/", Class::Task), root("RULES.md", Class::Rule)],
        policy: Policy::default(),
    };
    let wire = json!({
        "roots": [{"path": "tasks/", "class": "task"}, {"path": "RULES.md", "class": "rule"}],
        "policy": {
            "base_seconds": 60,
            "idle_after_seconds": 600,
            "task_idle_seconds": 300,
            "rule_idle_seconds": 600
        }
    });
    assert_eq!(
        serde_json::to_value(&typed).expect("a spec serializes"),
        wire
    );
    assert_eq!(
        serde_json::from_value::<Spec>(wire.clone()).expect("the wire spec deserializes"),
        typed
    );
    for pointer in ["", "/roots/0"] {
        let document = with_unknown_field(wire.clone(), pointer);
        assert!(
            serde_json::from_slice::<Spec>(&document).is_err(),
            "unknown field at {pointer:?}"
        );
    }
    assert!(serde_json::from_value::<Class>(json!("Task")).is_err());
}

#[test]
fn spec_validate_rejects_overlap_and_bad_paths() {
    let task = |path: &str| root(path, Class::Task);
    let cases = [
        (
            "file inside a directory root",
            vec![task("tasks/"), task("tasks/a.md")],
        ),
        (
            "directory root listed after its file",
            vec![task("tasks/a.md"), task("tasks/")],
        ),
        (
            "directory inside a directory root",
            vec![task("tasks/"), task("tasks/done/")],
        ),
        ("duplicate directory", vec![task("tasks/"), task("tasks/")]),
        (
            "same directory with another class",
            vec![task("tasks/"), root("tasks/", Class::Rule)],
        ),
        (
            "duplicate file",
            vec![root("RULES.md", Class::Rule), root("RULES.md", Class::Rule)],
        ),
        ("no roots", Vec::new()),
        ("65 roots", files(MAX_ROOTS + 1)),
    ];
    for (case, roots) in cases {
        assert_eq!(
            error(spec(roots).validate()).as_deref(),
            Some(INVALID_ROOTS),
            "{case}"
        );
    }
    // The brief leaves open whether a root failing relative_path reports the path or the roots.
    for path in ["/abs", "a/../b", "a//b", "a\\b", "", "./a", "a\0b"] {
        let message = error(spec(vec![task("RULES.md"), task(path)]).validate());
        assert!(
            matches!(message.as_deref(), Some(INVALID_ROOTS | INVALID_PATH)),
            "{path:?}: {message:?}"
        );
    }
    let policy = Policy {
        base_seconds: 59,
        ..Policy::default()
    };
    let expected = error(policy.validate());
    assert!(expected.is_some(), "the fixture policy must be invalid");
    let invalid = Spec {
        roots: vec![task("tasks/")],
        policy,
    };
    assert_eq!(error(invalid.validate()), expected);
}

#[test]
fn spec_validate_accepts_dir_and_file_roots() {
    let accepted = [
        vec![root("tasks/", Class::Task), root("RULES.md", Class::Rule)],
        // Sharing a name prefix is not being inside: neither file is under "tasks/".
        vec![
            root("tasks/", Class::Task),
            root("tasks.md", Class::Rule),
            root("tasks-done/a.md", Class::Task),
        ],
        vec![
            root("a/b/", Class::Task),
            root("a/c/", Class::Rule),
            root("a/d.md", Class::Rule),
        ],
        files(MAX_ROOTS),
    ];
    for roots in accepted {
        let candidate = spec(roots);
        assert_eq!(error(candidate.validate()), None, "{candidate:?}");
    }
}

#[test]
fn relative_path_accepts_files_and_directories() {
    let longest = "a".repeat(1024);
    let multibyte = format!("{}a", "あ".repeat(341));
    assert_eq!(multibyte.len(), 1024);
    for path in [
        "a",
        "RULES.md",
        "tasks/",
        "tasks/a.md",
        "a/b/c/",
        "notes/2026-11-07.md",
        longest.as_str(),
        multibyte.as_str(),
    ] {
        assert_eq!(error(relative_path(path)), None, "{path:?}");
    }
}

#[test]
fn relative_path_rejects_unsafe_paths() {
    let too_long = "a".repeat(1025);
    // 1,026 bytes in only 342 characters: the limit counts bytes.
    let multibyte = "あ".repeat(342);
    for path in [
        "",
        "/",
        "/abs",
        "//a",
        "a//b",
        "a/b//",
        "tasks//",
        ".",
        "..",
        "./",
        "../",
        "./a",
        "../a",
        "a/.",
        "a/..",
        "a/./b",
        "a/../b",
        "a\\b",
        "\\abs",
        "a\0b",
        "a\nb",
        "a\tb",
        "a\u{1f}b",
        "a\u{7f}b",
        too_long.as_str(),
        multibyte.as_str(),
    ] {
        assert_eq!(
            error(relative_path(path)).as_deref(),
            Some(INVALID_PATH),
            "{path:?}"
        );
    }
}

#[test]
fn bot_key_matches_vector() {
    // printf 'utsurigi-agent-hub live v1\0default\0alpha' | sha256sum
    assert_eq!(
        bot_key("default", "alpha"),
        "b5b34d8dbe2bd8441d0ec2f618a224f793040d9a9a26cf3fae5f5ee1e0582124"
    );
}

#[test]
fn hex_round_trip_and_unhex_rejects_bad_input() {
    assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    assert_eq!(hex(&[]), "");
    let every: Vec<u8> = (0..=255).collect();
    let text = hex(&every);
    assert_eq!(text.len(), 512);
    assert!(
        !text.bytes().any(|byte| byte.is_ascii_uppercase()),
        "{text}"
    );
    assert_eq!(must(unhex(&text)), every);
    assert_eq!(must(unhex("")), Vec::<u8>::new());
    for bad in [
        "0", "abc", "AB", "aB", "0g", "zz", "+1", "-1", " 1", "1 ", "0x", "é",
    ] {
        assert!(unhex(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn sha256_hex_is_lowercase_sha256() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
