//! Tests for the small-object store.
//! Authored from the Task 1 requirements before the implementation existed.
//! Local backend only; R2 cases live in head_tests.rs behind the offline curl.

#![cfg(unix)]

use super::{Condition, Fetched, MAX_OBJECT_BYTES, ObjectStore, Written};
use crate::config::Config;
use crate::error::Result;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Barrier, mpsc};
use std::time::Duration;

const KEY: &str = "uah-live/a";

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("object operation failed: {}", error.message))
}

fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("object operation must fail"),
        Err(error) => error.message,
    }
}

fn found(fetched: Fetched) -> (Vec<u8>, String) {
    match fetched {
        Fetched::Found { body, token } => (body, token),
        Fetched::Missing => panic!("expected Found, got Missing"),
        Fetched::NotModified => panic!("expected Found, got NotModified"),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A valid local-provider config and an existing, empty, private repository.
fn local() -> (tempfile::TempDir, Config, PathBuf) {
    let root = tempfile::Builder::new()
        .prefix("uah-objects-")
        .tempdir()
        .unwrap();
    let repository = root.path().join("repository");
    fs::create_dir(&repository).unwrap();
    fs::set_permissions(&repository, fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = Config {
        version: 1,
        hub: "objects-fixture".into(),
        ..Config::default()
    };
    config.storage.provider = "local".into();
    // Generous, so lock waits on a loaded host cannot time out mid-test.
    config.sync.stuck_request_timeout_seconds = 30;
    must(config.validate());
    (root, config, repository)
}

fn open(config: &Config, repository: &Path) -> ObjectStore {
    must(ObjectStore::new(config, repository.to_str().unwrap()))
}

#[test]
fn google_drive_uses_the_local_repository_path() {
    let (_root, mut config, repository) = local();
    config.storage.provider = "google-drive".into();
    must(config.validate());
    let store = open(&config, &repository);
    assert_eq!(must(store.put(KEY, b"x", Condition::Create)), Written::Yes);
    assert_eq!(fs::read(repository.join(KEY)).unwrap(), b"x");
}

#[test]
fn local_create_get_match_and_delete() {
    let (_root, config, repository) = local();
    let store = open(&config, &repository);
    assert_eq!(must(store.put(KEY, b"x", Condition::Create)), Written::Yes);
    assert_eq!(
        fs::read(repository.join(KEY)).unwrap(),
        b"x",
        "<repo>/<key>"
    );
    assert_eq!(
        must(store.put(KEY, b"y", Condition::Create)),
        Written::Conflict
    );
    let token = sha256_hex(b"x");
    assert_eq!(
        found(must(store.get(KEY, None, 1024))),
        (b"x".to_vec(), token.clone())
    );
    assert!(matches!(
        must(store.get(KEY, Some(&token), 1024)),
        Fetched::NotModified
    ));
    assert_eq!(
        found(must(store.get(KEY, Some("stale"), 1024))),
        (b"x".to_vec(), token.clone())
    );
    assert_eq!(
        must(store.put(KEY, b"replaced", Condition::Match(&token))),
        Written::Yes
    );
    assert_eq!(
        must(store.put(KEY, b"lost", Condition::Match("stale"))),
        Written::Conflict
    );
    assert_eq!(
        must(store.put(KEY, b"lost", Condition::Match(&token))),
        Written::Conflict,
        "a replaced token is stale"
    );
    assert_eq!(
        found(must(store.get(KEY, Some(&token), 1024))),
        (b"replaced".to_vec(), sha256_hex(b"replaced"))
    );
    // Writes go through a renamed temp file: nothing else may remain.
    assert_eq!(names(&repository.join("uah-live")), ["a"]);
    assert_eq!(names(&repository), ["uah-live", "uah-objects.lock"]);
    must(store.delete(KEY));
    must(store.delete(KEY));
    assert!(matches!(must(store.get(KEY, None, 1024)), Fetched::Missing));
    assert!(matches!(
        must(store.get(KEY, Some(&token), 1024)),
        Fetched::Missing
    ));
    assert_eq!(
        must(store.put(KEY, b"x", Condition::Match(&token))),
        Written::Conflict,
        "Match on an absent object"
    );
    assert!(matches!(must(store.get(KEY, None, 1024)), Fetched::Missing));
}

#[test]
fn local_rejects_bad_keys_and_limits() {
    let (root, config, repository) = local();
    let store = open(&config, &repository);
    let longest = format!("{}/{}/{}", "a".repeat(250), "b".repeat(250), "c".repeat(10));
    assert_eq!(longest.len(), 512);
    let too_long = format!("{longest}c");
    for key in [
        "",
        "../x",
        "a//b",
        "a/./b",
        "a b",
        "a/b/c/d/e/f/g/h/i",
        ".",
        "..",
        "/a",
        "a/",
        "a/..",
        "a\\b",
        "a%2Fb",
        "a\0b",
        "a\nb",
        "é",
        too_long.as_str(),
    ] {
        let invalid = "Invalid live object key";
        assert_eq!(failure(store.get(key, None, 1024)), invalid, "get {key:?}");
        assert_eq!(
            failure(store.put(key, b"x", Condition::Create)),
            invalid,
            "put {key:?}"
        );
        assert_eq!(failure(store.delete(key)), invalid, "delete {key:?}");
    }
    assert_eq!(names(root.path()), ["repository"]);
    assert!(
        names(&repository)
            .iter()
            .all(|name| name == "uah-objects.lock"),
        "a rejected key must not create objects"
    );
    for key in [
        "a/b/c/d/e/f/g/h",
        longest.as_str(),
        "A-Z_a.z/0-9",
        "..a/a..",
    ] {
        assert_eq!(
            must(store.put(key, b"ok", Condition::Create)),
            Written::Yes,
            "{key:?}"
        );
        assert_eq!(found(must(store.get(key, None, 1024))).0, b"ok");
        must(store.delete(key));
    }

    assert_eq!(
        must(store.put("uah-live/two", b"xy", Condition::Create)),
        Written::Yes
    );
    assert_eq!(found(must(store.get("uah-live/two", None, 2))).0, b"xy");
    assert_eq!(
        failure(store.get("uah-live/two", None, 1)),
        "Live object response exceeds its size limit"
    );
    assert!(
        store
            .get("uah-live/two", None, MAX_OBJECT_BYTES + 1)
            .is_err(),
        "a limit above MAX_OBJECT_BYTES must fail"
    );
    let largest = vec![b'z'; MAX_OBJECT_BYTES as usize];
    let oversized = [largest.as_slice(), &b"z"[..]].concat();
    assert!(
        store
            .put("uah-live/large", &oversized, Condition::Create)
            .is_err()
    );
    assert!(matches!(
        must(store.get("uah-live/large", None, MAX_OBJECT_BYTES)),
        Fetched::Missing
    ));
    assert_eq!(
        must(store.put("uah-live/large", &largest, Condition::Create)),
        Written::Yes
    );
    let (body, token) = found(must(store.get("uah-live/large", None, MAX_OBJECT_BYTES)));
    assert!(body == largest, "a MAX_OBJECT_BYTES object must round-trip");
    assert_eq!(token, sha256_hex(&largest));
    assert!(
        store
            .put("uah-live/large", &oversized, Condition::Match(&token))
            .is_err()
    );
    assert_eq!(
        found(must(store.get("uah-live/large", None, MAX_OBJECT_BYTES))).1,
        token
    );
}

#[test]
fn local_files_are_private() {
    let (root, config, repository) = local();
    let store = open(&config, &repository);
    let object = repository.join("uah-live/bot/list.json");
    assert_eq!(
        must(store.put("uah-live/bot/list.json", b"x", Condition::Create)),
        Written::Yes
    );
    for directory in ["uah-live", "uah-live/bot"] {
        let metadata = fs::symlink_metadata(repository.join(directory)).unwrap();
        assert!(metadata.is_dir());
        // A setgid bit inherited from the temp root grants nothing to others.
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700, "{directory}");
    }
    for file in [&object, &repository.join("uah-objects.lock")] {
        let metadata = fs::symlink_metadata(file).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(
            metadata.permissions().mode() & 0o7777,
            0o600,
            "{}",
            file.display()
        );
    }
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644)).unwrap();
    let token = sha256_hex(b"x");
    assert_eq!(
        must(store.put("uah-live/bot/list.json", b"y", Condition::Match(&token))),
        Written::Yes
    );
    assert_eq!(
        fs::symlink_metadata(&object).unwrap().permissions().mode() & 0o7777,
        0o600,
        "a replaced object must be private"
    );
    // Parents are created only inside an existing repository directory.
    let absent = root.path().join("absent-repository");
    assert!(
        ObjectStore::new(&config, absent.to_str().unwrap())
            .and_then(|store| store.put(KEY, b"x", Condition::Create))
            .is_err()
    );
    assert!(!absent.exists());
    // The repository URL itself must pass Config::validate_url.
    let target = root.path().join("linked-target");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.path().join("linked-repository")).unwrap();
    let linked = root.path().join("linked-repository");
    for repository in ["relative/repository", linked.to_str().unwrap()] {
        assert!(
            ObjectStore::new(&config, repository)
                .and_then(|store| store.put(KEY, b"x", Condition::Create))
                .is_err(),
            "accepted repository {repository}"
        );
    }
    assert_eq!(names(&target), Vec::<String>::new());
}

#[test]
fn local_rejects_symlinked_object() {
    let (root, config, repository) = local();
    let store = open(&config, &repository);
    assert_eq!(must(store.put(KEY, b"x", Condition::Create)), Written::Yes);
    let sentinel = root.path().join("sentinel");
    fs::write(&sentinel, b"synthetic sentinel").unwrap();
    let object = repository.join(KEY);
    fs::remove_file(&object).unwrap();
    symlink(&sentinel, &object).unwrap();
    assert!(
        store.get(KEY, None, 1024).is_err(),
        "a symlinked object must not be read"
    );
    let token = sha256_hex(b"synthetic sentinel");
    for condition in [Condition::Create, Condition::Match(&token)] {
        assert!(
            !matches!(store.put(KEY, b"y", condition), Ok(Written::Yes)),
            "a symlinked object must not be replaced"
        );
    }
    assert_eq!(fs::read(&sentinel).unwrap(), b"synthetic sentinel");

    fs::remove_file(&object).unwrap();
    fs::create_dir(&object).unwrap();
    assert!(store.get(KEY, None, 1024).is_err(), "only regular files");
    fs::remove_dir(&object).unwrap();
    let fifo = CString::new(object.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let rejected = open(&config, &repository).get(KEY, None, 1024).is_err();
            sender.send(rejected).unwrap();
        });
        let rejected = receiver.recv_timeout(Duration::from_secs(10));
        if rejected.is_err() {
            // Release a reader blocked on the FIFO so the scope can end.
            drop(
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&object),
            );
        }
        assert_eq!(
            rejected,
            Ok(true),
            "a FIFO must be rejected without blocking"
        );
    });

    fs::remove_file(&object).unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("b"), b"outside").unwrap();
    symlink(&outside, repository.join("linked")).unwrap();
    assert!(
        store.get("linked/b", None, 1024).is_err(),
        "a symlinked parent must not be followed"
    );
    assert!(store.put("linked/c", b"x", Condition::Create).is_err());
    assert_eq!(names(&outside), ["b"]);
}

#[test]
fn local_put_and_delete_wait_for_the_object_lock() {
    let (_root, config, repository) = local();
    assert_eq!(
        must(open(&config, &repository).put(KEY, b"x", Condition::Create)),
        Written::Yes
    );
    let lock = File::options()
        .read(true)
        .write(true)
        .open(repository.join("uah-objects.lock"))
        .unwrap();
    lock.lock().unwrap();
    std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            open(&config, &repository)
                .put("uah-live/b", b"y", Condition::Create)
                .map_err(|error| error.message)
        });
        let deleter = scope.spawn(|| {
            open(&config, &repository)
                .delete(KEY)
                .map_err(|error| error.message)
        });
        std::thread::sleep(Duration::from_millis(300));
        let waited = !repository.join("uah-live/b").exists() && repository.join(KEY).exists();
        lock.unlock().unwrap();
        assert!(waited, "put and delete must hold <repo>/uah-objects.lock");
        assert_eq!(writer.join().unwrap(), Ok(Written::Yes));
        assert_eq!(deleter.join().unwrap(), Ok(()));
    });
    assert!(!repository.join(KEY).exists());
}

#[test]
fn local_concurrent_conditional_writes_have_one_winner() {
    let (_root, config, repository) = local();
    let mut token: Option<String> = None;
    for round in 0..2 {
        let barrier = Barrier::new(8);
        let outcomes: Vec<Written> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let (barrier, config, repository) = (&barrier, &config, &repository);
                    let token = token.as_deref();
                    scope.spawn(move || {
                        let store = open(config, repository);
                        let condition = match token {
                            Some(token) => Condition::Match(token),
                            None => Condition::Create,
                        };
                        barrier.wait(); // Every writer holds the same precondition.
                        let body = format!("writer {round}.{index}");
                        must(store.put(KEY, body.as_bytes(), condition))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        let winners: Vec<_> = (0..8)
            .filter(|index| outcomes[*index] == Written::Yes)
            .collect();
        assert_eq!(winners.len(), 1, "round {round} must admit one writer");
        assert!(
            outcomes
                .iter()
                .all(|outcome| *outcome == Written::Yes || *outcome == Written::Conflict)
        );
        let body = format!("writer {round}.{}", winners[0]);
        assert_eq!(fs::read(repository.join(KEY)).unwrap(), body.as_bytes());
        token = Some(sha256_hex(body.as_bytes()));
    }
}
