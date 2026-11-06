//! Independent tests authored against PLAN Task 2 on d6478ae, without reading
//! the head implementation. Include from head.rs with #[path = "head_tests.rs"].
//! Every case runs in its own bounded process: no process-wide environment
//! mutation can race another test, and a broken FIFO/lock/transport cannot hang
//! the test runner. The curl fixture is an offline boundary, never a real curl.

#![cfg(unix)]

use super::{Head, HeadStore, ObservedHead};
use crate::config::{CHILD_ENV, Config};
use crate::error::Result;
use crate::objects::{Condition, Fetched, MAX_OBJECT_BYTES, ObjectStore, Written};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const HUB: &str = "head-fixture";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const REPOSITORY: &str = "s3:https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/ownerless-head-fixture/shared/prefix";
const OBJECT: &str = "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/ownerless-head-fixture/shared/prefix/uah-head.json";
const ETAG: &str = "\"opaque.+/==:v1\"";
const ACCESS: &str = "synthetic-access\"\\fixture";
const SECRET: &str = "synthetic-secret\\\"fixture";
const SESSION: &str = "synthetic-session\"\\fixture";
const SHARED_PASSWORD: &str = "synthetic-shared-password";
const BOT_PASSWORD: &str = "synthetic-bot-password";
const PARENT_SECRET: &str = "synthetic-parent-secret";
const CURL: &str = include_str!("../tests/fixtures/ownerless-head-curl.py");

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("head operation failed: {}", error.message))
}

fn head(revision: u64, snapshot: &str) -> Head {
    Head {
        version: 2,
        hub: HUB.into(),
        revision,
        snapshot: snapshot.into(),
    }
}

fn document(revision: u64, snapshot: &str) -> Value {
    // Expected wire values come from the requirements, not product serializers.
    json!({"version": 2, "hub": HUB, "revision": revision, "snapshot": snapshot})
}

fn same(actual: &Head, expected: &Head) {
    assert!(
        actual == expected,
        "head must equal the entire intended value; actual={}, expected={}",
        serde_json::to_string(actual).unwrap(),
        serde_json::to_string(expected).unwrap()
    );
}

fn private_write(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.write_all(bytes).unwrap();
}

fn config(root: &Path, provider: &str) -> Config {
    let mut config = Config {
        version: 1,
        hub: HUB.into(),
        timezone: Some("UTC".into()),
        ..Config::default()
    };
    config.storage.provider = provider.into();
    config.bot.id = Some("alpha".into());
    config.bot.platform = Some("synthetic-test".into());
    config.bot.runtime_id = Some("alpha-runtime".into());
    config.repositories.shared.url = Some(if provider == "local" {
        root.join("repository").to_str().unwrap().into()
    } else {
        REPOSITORY.into()
    });
    config.repositories.bot.url = Some(if provider == "local" {
        root.join("bot-repository").to_str().unwrap().into()
    } else {
        REPOSITORY.replace("/shared/prefix", "/bots/alpha")
    });
    config.repositories.shared.password_env = Some("OWNERLESS_SHARED_PASSWORD".into());
    config.repositories.bot.password_env = Some("OWNERLESS_BOT_PASSWORD".into());
    config.machine.root = Some(root.join("machine"));
    config.paths.owned = Some(root.join("owned"));
    config.paths.shared = Some(root.join("shared"));
    config.paths.imports = Some(root.join("imports"));
    config.paths.state = Some(root.join("state"));
    config.data.include = vec![PathBuf::from(".")];
    config.sync.stuck_request_timeout_seconds = 2;
    for directory in [
        "repository",
        "bot-repository",
        "machine",
        "owned",
        "shared",
        "imports",
        "state",
    ] {
        let path = root.join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    must(config.validate());
    must(config.runtime());
    config
}

struct ProcessGroup(Child);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The worker has its own process group; remove abandoned fake curls too.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

fn worker_output(path: &Path) -> String {
    let mut output = String::new();
    File::open(path)
        .unwrap()
        .take(256 * 1024)
        .read_to_string(&mut output)
        .unwrap();
    output
}

fn isolated(case: &str, body: impl FnOnce(&Path)) {
    if std::env::var("OWNERLESS_HEAD_CASE").as_deref() == Ok(case) {
        let root = PathBuf::from(std::env::var_os("OWNERLESS_HEAD_ROOT").unwrap());
        body(&root);
        private_write(&root.join("case.completed"), case.as_bytes());
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("ownerless-head-")
        .tempdir()
        .unwrap();
    let bin = root.path().join("bin");
    let home = root.path().join("synthetic-home");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&home).unwrap();
    let curl = bin.join("curl");
    private_write(&curl, CURL.as_bytes());
    fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
    private_write(
        &home.join(".curlrc"),
        b"url = https://ambient-config-must-be-disabled.invalid/\n",
    );
    for name in ["ca.pem", "client.pem"] {
        private_write(&root.path().join(name), b"synthetic certificate fixture\n");
    }
    let stdout_path = root.path().join("worker.stdout");
    let stderr_path = root.path().join("worker.stderr");
    let module = module_path!().split_once("::").unwrap().1;
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            &format!("{module}::{case}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(root.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", &home)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC")
        .env("OWNERLESS_HEAD_CASE", case)
        .env("OWNERLESS_HEAD_ROOT", root.path())
        .env("AWS_ACCESS_KEY_ID", ACCESS)
        .env("AWS_SECRET_ACCESS_KEY", SECRET)
        .env("AWS_SESSION_TOKEN", SESSION)
        .env("OWNERLESS_SHARED_PASSWORD", SHARED_PASSWORD)
        .env("OWNERLESS_BOT_PASSWORD", BOT_PASSWORD)
        .env("RESTIC_PASSWORD", SHARED_PASSWORD)
        .env("OWNERLESS_PARENT_SECRET", PARENT_SECRET)
        .env("RESTIC_CACERT", root.path().join("ca.pem"))
        .env("RESTIC_TLS_CLIENT_CERT", root.path().join("client.pem"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(File::create(&stderr_path).unwrap()))
        .process_group(0);
    for name in CHILD_ENV {
        if name == "PATH" {
            continue;
        }
        let value = if name.to_ascii_lowercase().ends_with("proxy") {
            if name.eq_ignore_ascii_case("NO_PROXY") {
                String::new()
            } else {
                "http://127.0.0.1:9".into()
            }
        } else {
            root.path().join(name).to_str().unwrap().into()
        };
        command.env(name, value);
    }
    let mut worker = ProcessGroup(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = worker.0.try_wait().unwrap() {
            break status;
        }
        let excessive_output = [&stdout_path, &stderr_path]
            .iter()
            .any(|path| fs::metadata(path).unwrap().len() > 256 * 1024);
        if Instant::now() >= deadline || excessive_output {
            panic!(
                "{case} exceeded its process/output bound\n{}\n{}",
                worker_output(&stdout_path),
                worker_output(&stderr_path)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    for path in [&stdout_path, &stderr_path] {
        assert!(fs::metadata(path).unwrap().len() <= 256 * 1024);
    }
    assert!(
        status.success(),
        "{case} worker failed ({status})\n{}\n{}",
        worker_output(&stdout_path),
        worker_output(&stderr_path)
    );
    assert_eq!(
        fs::read_to_string(root.path().join("case.completed")).unwrap(),
        case,
        "a zero-test subprocess must never count as a completed case"
    );
}

fn current(store: &HeadStore) -> ObservedHead {
    must(store.read()).expect("head must exist")
}

#[test]
fn local_absence_create_replace_and_stale_compare() {
    isolated("local_absence_create_replace_and_stale_compare", |root| {
        let config = config(root, "local");
        let store = must(HeadStore::new(&config));
        assert!(must(store.read()).is_none());
        assert!(must(store.compare_exchange(None, &head(1, A))));
        let first = current(&store);
        same(&first.head, &head(1, A));
        assert!(!first.token.is_empty());
        assert_eq!(current(&store).token, first.token);
        assert!(!must(store.compare_exchange(None, &head(1, B))));
        assert!(must(store.compare_exchange(Some(&first), &head(2, B))));
        let second = current(&store);
        same(&second.head, &head(2, B));
        assert_ne!(second.token, first.token);
        assert!(!must(store.compare_exchange(Some(&first), &head(2, C))));
        same(&current(&store).head, &head(2, B));
        let reopened = must(HeadStore::new(&config));
        same(&current(&reopened).head, &head(2, B));
        assert_eq!(current(&reopened).token, second.token);
        let disk: Value =
            serde_json::from_slice(&fs::read(root.join("repository/uah-head.json")).unwrap())
                .unwrap();
        assert_eq!(disk, document(2, B));
    });
}

fn contend(root: &Path, replacement: bool) {
    let config = config(root, "local");
    let store = must(HeadStore::new(&config));
    if replacement {
        assert!(must(store.compare_exchange(None, &head(1, A))));
    }
    let barrier = Arc::new(Barrier::new(8));
    let outcomes = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let barrier = Arc::clone(&barrier);
                let config = &config;
                scope.spawn(move || {
                    let store = must(HeadStore::new(config));
                    let expected = must(store.read());
                    let next = head(
                        if replacement { 2 } else { 1 },
                        &format!("{:064x}", index + 16),
                    );
                    barrier.wait(); // All contenders observe the same pre-CAS head.
                    let won = must(store.compare_exchange(expected.as_ref(), &next));
                    (won, next)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let winners: Vec<_> = outcomes.iter().filter(|(won, _)| *won).collect();
    assert_eq!(winners.len(), 1, "CAS must admit exactly one writer");
    same(&current(&store).head, &winners[0].1);
}

#[test]
fn local_concurrent_create_has_one_winner() {
    isolated("local_concurrent_create_has_one_winner", |root| {
        contend(root, false)
    });
}

#[test]
fn local_concurrent_replacement_has_one_winner() {
    isolated("local_concurrent_replacement_has_one_winner", |root| {
        contend(root, true)
    });
}

#[test]
fn local_cas_has_one_winner_across_independent_processes() {
    isolated(
        "local_cas_has_one_winner_across_independent_processes",
        |root| {
            let mut config = config(root, "local");
            let role = std::env::var("OWNERLESS_HEAD_CONTENDER").ok();
            if let Some(role) = &role {
                // Separate runtimes/state directories still share one backend
                // lock. A process mutex or per-runtime lock cannot suffice.
                let state = root.join(format!("state-{role}"));
                fs::create_dir_all(&state).unwrap();
                fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
                config.paths.state = Some(state);
                config.bot.runtime_id = Some(format!("runtime-{role}"));
                must(config.validate());
            }
            let store = must(HeadStore::new(&config));
            if let Some(role) = role {
                let observed = current(&store);
                same(&observed.head, &head(1, A));
                private_write(&root.join(format!("ready-{role}")), b"ready");
                let start = Instant::now();
                while !root.join("release-contenders").exists() {
                    assert!(start.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(10));
                }
                let next = head(2, if role == "0" { B } else { C });
                let won = must(store.compare_exchange(Some(&observed), &next));
                private_write(
                    &root.join(format!("won-{role}")),
                    if won { b"true" } else { b"false" },
                );
                return;
            }
            assert!(must(store.compare_exchange(None, &head(1, A))));
            let module = module_path!().split_once("::").unwrap().1;
            let mut contenders: Vec<_> = (0..2)
                .map(|role| {
                    Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            &format!(
                                "{module}::local_cas_has_one_winner_across_independent_processes"
                            ),
                            "--nocapture",
                            "--test-threads=1",
                        ])
                        .env("OWNERLESS_HEAD_CONTENDER", role.to_string())
                        .stdin(Stdio::null())
                        .stdout(Stdio::from(
                            File::create(root.join(format!("contender-{role}.stdout"))).unwrap(),
                        ))
                        .stderr(Stdio::from(
                            File::create(root.join(format!("contender-{role}.stderr"))).unwrap(),
                        ))
                        .spawn()
                        .unwrap()
                })
                .collect();
            let start = Instant::now();
            while !(root.join("ready-0").exists() && root.join("ready-1").exists()) {
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(10));
            }
            // A file barrier releases both processes only after each has read
            // revision 1. The outer process-group watchdog bounds contenders
            // and cleans them up if a lock hangs.
            private_write(&root.join("release-contenders"), b"go");
            for (role, contender) in contenders.iter_mut().enumerate() {
                assert!(
                    contender.wait().unwrap().success(),
                    "{}",
                    worker_output(&root.join(format!("contender-{role}.stderr")))
                );
            }
            let wins: Vec<_> = (0..2)
                .map(|role| fs::read_to_string(root.join(format!("won-{role}"))).unwrap())
                .collect();
            assert_eq!(wins.iter().filter(|won| won.as_str() == "true").count(), 1);
            assert!(wins.iter().all(|won| won == "true" || won == "false"));
            same(
                &current(&store).head,
                &head(2, if wins[0] == "true" { B } else { C }),
            );
        },
    );
}

#[test]
fn local_readers_observe_complete_heads_during_replacement() {
    isolated(
        "local_readers_observe_complete_heads_during_replacement",
        |root| {
            let config = config(root, "local");
            let store = must(HeadStore::new(&config));
            assert!(must(store.compare_exchange(None, &head(1, A))));
            let start = Barrier::new(5);
            let finish = Barrier::new(5);
            std::thread::scope(|scope| {
                let readers: Vec<_> = (0..4)
                    .map(|_| {
                        scope.spawn(|| {
                            let reader = must(HeadStore::new(&config));
                            for old in 1..=32 {
                                start.wait();
                                let seen = current(&reader);
                                let before = head(old, if old % 2 == 1 { A } else { B });
                                let after = head(old + 1, if old % 2 == 1 { B } else { A });
                                assert!(seen.head == before || seen.head == after);
                                finish.wait();
                            }
                        })
                    })
                    .collect();
                for old in 1..=32 {
                    let observed = current(&store);
                    start.wait();
                    let next = head(old + 1, if old % 2 == 1 { B } else { A });
                    assert!(must(store.compare_exchange(Some(&observed), &next)));
                    finish.wait();
                }
                for reader in readers {
                    reader.join().unwrap();
                }
            });
            same(&current(&store).head, &head(33, A));
        },
    );
}

fn malformed_documents() -> Vec<String> {
    let valid = document(1, A);
    let mut documents = vec![String::new(), "{".into(), "null".into(), "[]".into()];
    for (field, value) in [
        ("version", json!(1)),
        ("version", json!(3)),
        ("version", json!("2")),
        ("hub", json!("other-hub")),
        ("hub", json!("../head-fixture")),
        ("hub", json!("")),
        ("hub", json!("x".repeat(129))),
        ("revision", json!(0)),
        ("revision", json!(-1)),
        ("revision", json!(1.5)),
        ("revision", json!("1")),
        ("snapshot", json!("")),
        ("snapshot", json!("a".repeat(63))),
        ("snapshot", json!("a".repeat(65))),
        ("snapshot", json!("A".repeat(64))),
        ("snapshot", json!("g".repeat(64))),
        ("snapshot", json!("a\nb")),
        ("unexpected", json!(true)),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        documents.push(invalid.to_string());
    }
    for field in ["version", "hub", "revision", "snapshot"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        documents.push(missing.to_string());
        documents.push(format!(
            "{{\"{field}\":{},{}",
            valid[field],
            &valid.to_string()[1..]
        ));
    }
    documents.push(format!("{} trailing", valid));
    documents.push(valid.to_string() + &" ".repeat(16 * 1024));
    documents
}

#[test]
fn local_rejects_malformed_unknown_duplicate_and_oversized_heads() {
    isolated(
        "local_rejects_malformed_unknown_duplicate_and_oversized_heads",
        |root| {
            let config = config(root, "local");
            for (index, body) in malformed_documents().iter().enumerate() {
                private_write(&root.join("repository/uah-head.json"), body.as_bytes());
                let result = HeadStore::new(&config).and_then(|store| store.read());
                assert!(result.is_err(), "invalid document {index} was accepted");
            }
        },
    );
}

#[test]
fn local_rejects_invalid_proposals_and_nonunit_revisions() {
    isolated(
        "local_rejects_invalid_proposals_and_nonunit_revisions",
        |root| {
            let config = config(root, "local");
            let store = must(HeadStore::new(&config));
            let mut invalid = vec![head(0, A), head(2, A), head(1, "short")];
            let mut wrong_version = head(1, A);
            wrong_version.version = 1;
            invalid.push(wrong_version);
            let mut wrong_hub = head(1, A);
            wrong_hub.hub = "another".into();
            invalid.push(wrong_hub);
            for proposal in invalid {
                assert!(store.compare_exchange(None, &proposal).is_err());
                assert!(must(store.read()).is_none());
            }
            assert!(must(store.compare_exchange(None, &head(1, A))));
            let observed = current(&store);
            for revision in [0, 1, 3, u64::MAX] {
                assert!(
                    store
                        .compare_exchange(Some(&observed), &head(revision, B))
                        .is_err()
                );
                same(&current(&store).head, &head(1, A));
            }
            private_write(
                &root.join("repository/uah-head.json"),
                document(u64::MAX, A).to_string().as_bytes(),
            );
            let last = current(&store);
            assert!(store.compare_exchange(Some(&last), &head(0, B)).is_err());
            same(&current(&store).head, &head(u64::MAX, A));
        },
    );
}

fn local_files(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("repository"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

fn lock_path(root: &Path) -> PathBuf {
    let locks: Vec<_> = local_files(root)
        .into_iter()
        .filter(|path| path.file_name().unwrap() != "uah-head.json")
        .collect();
    assert!(
        !locks.is_empty(),
        "a persistent OS lock must guard the head"
    );
    locks[0].clone()
}

#[test]
fn local_head_and_lock_are_regular_private_files() {
    isolated("local_head_and_lock_are_regular_private_files", |root| {
        let config = config(root, "local");
        let store = must(HeadStore::new(&config));
        assert!(must(store.compare_exchange(None, &head(1, A))));
        let observed = current(&store);
        assert!(must(store.compare_exchange(Some(&observed), &head(2, B))));
        let _ = lock_path(root);
        for path in local_files(root) {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(metadata.file_type().is_file());
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        }
    });
}

#[test]
fn local_existing_head_and_lock_never_remain_public_after_accepted_cas() {
    isolated(
        "local_existing_head_and_lock_never_remain_public_after_accepted_cas",
        |root| {
            let config = config(root, "local");
            let store = must(HeadStore::new(&config));
            assert!(must(store.compare_exchange(None, &head(1, A))));
            let paths = [root.join("repository/uah-head.json"), lock_path(root)];
            for path in paths {
                let observed = current(&store);
                let proposal = head(observed.head.revision + 1, B);
                fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                let result = HeadStore::new(&config)
                    .and_then(|store| store.compare_exchange(Some(&observed), &proposal));
                if let Ok(won) = result {
                    assert!(won);
                    same(&current(&store).head, &proposal);
                    assert_eq!(
                        fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                        0o600
                    );
                }
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        },
    );
}

#[test]
fn local_rejects_head_symlinks_directories_and_fifos() {
    isolated(
        "local_rejects_head_symlinks_directories_and_fifos",
        |root| {
            let config = config(root, "local");
            let target = root.join("synthetic-sentinel");
            private_write(&target, document(1, A).to_string().as_bytes());
            let before = fs::read(&target).unwrap();
            let path = root.join("repository/uah-head.json");
            symlink(&target, &path).unwrap();
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
            assert!(
                HeadStore::new(&config)
                    .and_then(|s| s.compare_exchange(None, &head(1, B)))
                    .is_err()
            );
            assert_eq!(fs::read(&target).unwrap(), before);
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
            fs::remove_dir(&path).unwrap();
            let name = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
        },
    );
}

#[test]
fn local_rejects_lock_symlinks_and_fifos() {
    isolated("local_rejects_lock_symlinks_and_fifos", |root| {
        let config = config(root, "local");
        let observed = {
            let store = must(HeadStore::new(&config));
            assert!(must(store.compare_exchange(None, &head(1, A))));
            current(&store)
        };
        let lock = lock_path(root);
        fs::remove_file(&lock).unwrap();
        let sentinel = root.join("lock-sentinel");
        private_write(&sentinel, b"do not change this synthetic sentinel");
        let before = fs::read(&sentinel).unwrap();
        symlink(&sentinel, &lock).unwrap();
        assert!(
            HeadStore::new(&config)
                .and_then(|s| s.compare_exchange(Some(&observed), &head(2, B)))
                .is_err()
        );
        assert_eq!(fs::read(&sentinel).unwrap(), before);
        fs::remove_file(&lock).unwrap();
        let name = CString::new(lock.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            HeadStore::new(&config)
                .and_then(|s| s.compare_exchange(Some(&observed), &head(2, B)))
                .is_err()
        );
    });
}

#[test]
fn local_rejects_repository_parent_symlinks() {
    isolated("local_rejects_repository_parent_symlinks", |root| {
        let mut config = config(root, "local");
        let target = root.join("actual-repository");
        fs::create_dir(&target).unwrap();
        let link = root.join("repository-link");
        symlink(&target, &link).unwrap();
        config.repositories.shared.url = Some(link.to_str().unwrap().into());
        assert!(
            HeadStore::new(&config)
                .and_then(|s| s.compare_exchange(None, &head(1, A)))
                .is_err()
        );
        assert_eq!(fs::read_dir(target).unwrap().count(), 0);
    });
}

fn fixture(root: &Path, remote: Value, behavior: Value) {
    private_write(&root.join("remote.json"), remote.to_string().as_bytes());
    private_write(&root.join("fixture.json"), behavior.to_string().as_bytes());
    let _ = fs::remove_file(root.join("calls.jsonl"));
}

fn present(revision: u64, snapshot: &str) -> Value {
    json!({"exists": true, "etag": ETAG, "head": document(revision, snapshot)})
}

fn absent() -> Value {
    json!({"exists": false, "etag": "\"absent\""})
}

fn calls(root: &Path) -> Vec<Value> {
    match fs::read_to_string(root.join("calls.jsonl")) {
        Ok(text) => text
            .lines()
            .map(|line| {
                let call: Value = serde_json::from_str(line).unwrap();
                assert!(call.get("fixture_error").is_none(), "{call}");
                call
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("cannot read synthetic curl calls: {error}"),
    }
}

fn option<'a>(call: &'a Value, key: &str) -> Option<&'a str> {
    call["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pair| pair[0] == key)
        .map(|pair| pair[1].as_str().unwrap())
}

fn put_calls(root: &Path) -> Vec<Value> {
    calls(root)
        .into_iter()
        .filter(|call| call["method"] == "PUT")
        .inspect(|call| {
            let headers = call["headers"].as_object().unwrap();
            assert_ne!(
                headers.contains_key("if-match"),
                headers.contains_key("if-none-match")
            );
            assert!(option(call, "retry").is_none_or(|value| value == "0"));
        })
        .collect()
}

fn check_transport(root: &Path, timeout: f64) {
    check_transport_to(root, timeout, &[OBJECT]);
}

fn check_transport_to(root: &Path, timeout: f64, urls: &[&str]) {
    let calls = calls(root);
    assert!(
        !calls.is_empty(),
        "the fake curl must actually be exercised"
    );
    for call in calls {
        assert_eq!(call["argv"][0], "-q");
        let url = option(&call, "url");
        assert!(
            url.is_some_and(|url| urls.contains(&url)),
            "unexpected URL {url:?}"
        );
        assert_eq!(
            option(&call, "user"),
            Some(format!("{ACCESS}:{SECRET}").as_str())
        );
        let signing: Vec<_> = option(&call, "aws-sigv4").unwrap().split(':').collect();
        assert_eq!(signing.len(), 4);
        assert_eq!(signing[0], "aws");
        assert_eq!(signing[1], "amz");
        assert!(!signing[2].is_empty());
        assert_eq!(signing[3], "s3");
        assert_eq!(call["headers"]["x-amz-security-token"], json!([SESSION]));
        let maximum: f64 = option(&call, "max-time").unwrap().parse().unwrap();
        assert!(maximum > 0.0 && maximum <= timeout.min(120.0));
        let argv = call["argv"].to_string();
        for secret in [
            ACCESS,
            SECRET,
            SESSION,
            SHARED_PASSWORD,
            BOT_PASSWORD,
            PARENT_SECRET,
        ] {
            assert!(!argv.contains(secret));
            for argument in call["argv"].as_array().unwrap() {
                assert!(!argument.as_str().unwrap().contains(secret));
            }
        }
        for forbidden in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "OWNERLESS_SHARED_PASSWORD",
            "OWNERLESS_BOT_PASSWORD",
            "RESTIC_PASSWORD",
            "OWNERLESS_PARENT_SECRET",
            "OWNERLESS_HEAD_CASE",
            "OWNERLESS_HEAD_ROOT",
            "HOME",
        ] {
            assert!(
                call["environment"].get(forbidden).is_none(),
                "leaked {forbidden}"
            );
        }
        for name in CHILD_ENV {
            assert_eq!(
                call["environment"][name].as_str(),
                Some(std::env::var(name).unwrap().as_str()),
                "connection setting {name} was lost"
            );
        }
        assert_eq!(
            option(&call, "cacert"),
            Some(root.join("ca.pem").to_str().unwrap())
        );
        assert_eq!(
            option(&call, "cert"),
            Some(root.join("client.pem").to_str().unwrap())
        );
        for file in call["files"].as_array().unwrap() {
            assert_eq!(file["regular"], true);
            assert_eq!(file["mode"], 0o600);
        }
    }
}

#[test]
fn r2_absence_create_and_exact_quoted_etag_replacement() {
    isolated(
        "r2_absence_create_and_exact_quoted_etag_replacement",
        |root| {
            let config = config(root, "r2");
            fixture(root, absent(), json!({}));
            let store = must(HeadStore::new(&config));
            assert!(must(store.read()).is_none());
            assert!(must(store.compare_exchange(None, &head(1, A))));
            let first = current(&store);
            same(&first.head, &head(1, A));
            assert_eq!(first.token, "\"new.etag/+==:2\"");
            assert!(must(store.compare_exchange(Some(&first), &head(2, B))));
            same(&current(&store).head, &head(2, B));
            let puts = put_calls(root);
            assert_eq!(puts.len(), 2);
            assert_eq!(puts[0]["headers"]["if-none-match"], json!(["*"]));
            assert!(puts[0]["headers"].get("if-match").is_none());
            assert_eq!(puts[1]["headers"]["if-match"], json!([first.token]));
            assert!(puts[1]["headers"].get("if-none-match").is_none());
            assert_eq!(
                serde_json::from_str::<Value>(puts[0]["body"].as_str().unwrap()).unwrap(),
                document(1, A)
            );
            assert_eq!(
                serde_json::from_str::<Value>(puts[1]["body"].as_str().unwrap()).unwrap(),
                document(2, B)
            );
            check_transport(root, 2.0);
        },
    );
}

#[test]
fn r2_replays_the_observed_opaque_etag_without_normalizing() {
    isolated(
        "r2_replays_the_observed_opaque_etag_without_normalizing",
        |root| {
            let config = config(root, "r2");
            fixture(root, present(1, A), json!({}));
            let store = must(HeadStore::new(&config));
            let observed = current(&store);
            assert_eq!(observed.token, ETAG);
            assert!(must(store.compare_exchange(Some(&observed), &head(2, B))));
            let puts = put_calls(root);
            assert_eq!(puts.len(), 1);
            assert_eq!(puts[0]["headers"]["if-match"], json!([ETAG]));
            check_transport(root, 2.0);
        },
    );
}

#[test]
fn r2_only_http_404_is_absence() {
    isolated("r2_only_http_404_is_absence", |root| {
        let config = config(root, "r2");
        for status in [201, 204, 206, 301, 304, 401, 403, 408, 412, 429, 500, 503] {
            fixture(root, present(1, A), json!({"get": {"status": status}}));
            assert!(
                HeadStore::new(&config).and_then(|s| s.read()).is_err(),
                "HTTP {status} is not absence"
            );
            assert!(put_calls(root).is_empty());
            check_transport(root, 2.0);
        }
        fixture(root, absent(), json!({"get": {"status": 0, "exit": 7}}));
        assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
        fixture(
            root,
            present(1, A),
            json!({"get": {"status": 200, "exit": 56}}),
        );
        assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
        fixture(root, absent(), json!({"get": {"status": 404}}));
        assert!(must(HeadStore::new(&config).and_then(|s| s.read())).is_none());
    });
}

#[test]
fn r2_accepts_valid_heads_at_the_body_limit_and_rejects_one_more_byte() {
    isolated(
        "r2_accepts_valid_heads_at_the_body_limit_and_rejects_one_more_byte",
        |root| {
            let config = config(root, "r2");
            let json = document(1, A).to_string();
            let at_limit = json.clone() + &" ".repeat(16 * 1024 - json.len());
            assert_eq!(at_limit.len(), 16 * 1024);
            fixture(root, present(1, A), json!({"get": {"body": at_limit}}));
            let store = must(HeadStore::new(&config));
            same(&current(&store).head, &head(1, A));
            fixture(
                root,
                present(1, A),
                json!({"get": {"body": at_limit + " "}}),
            );
            assert!(
                store.read().is_err(),
                "valid JSON over the body limit must fail"
            );
        },
    );
}

#[test]
fn r2_rejects_invalid_missing_duplicate_and_weak_etags() {
    isolated(
        "r2_rejects_invalid_missing_duplicate_and_weak_etags",
        |root| {
            let config = config(root, "r2");
            for etags in [
                json!([]),
                json!(["unquoted"]),
                json!(["W/\"weak\""]),
                json!([ETAG, ETAG]),
                json!(["\"first\"", "\"second\""]),
                json!(["\"unterminated"]),
                json!(["\"valid\"junk"]),
                json!(["\"bad\u{1}\""]),
                json!(["\"bad\rvalue\""]),
            ] {
                fixture(root, present(1, A), json!({"get": {"etags": etags}}));
                assert!(
                    HeadStore::new(&config).and_then(|s| s.read()).is_err(),
                    "accepted ETag set {etags}"
                );
                assert!(put_calls(root).is_empty());
            }
        },
    );
}

#[test]
fn r2_rejects_malformed_wrong_hub_and_oversized_heads() {
    isolated(
        "r2_rejects_malformed_wrong_hub_and_oversized_heads",
        |root| {
            let config = config(root, "r2");
            for (index, body) in malformed_documents().iter().enumerate() {
                fixture(root, present(1, A), json!({"get": {"body": body}}));
                assert!(
                    HeadStore::new(&config).and_then(|s| s.read()).is_err(),
                    "accepted document {index}"
                );
            }
            fixture(
                root,
                present(1, A),
                json!({"get": {"header_bytes": 64 * 1024 + 1}}),
            );
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
            fixture(
                root,
                present(1, A),
                json!({"get": {"body_bytes": 16 * 1024 + 1}}),
            );
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
        },
    );
}

#[test]
fn r2_http_412_is_contention_without_unconditional_fallback() {
    isolated(
        "r2_http_412_is_contention_without_unconditional_fallback",
        |root| {
            let config = config(root, "r2");
            for replacement in [false, true] {
                fixture(
                    root,
                    if replacement { present(1, A) } else { absent() },
                    json!({"put": {"status": 412}}),
                );
                let store = must(HeadStore::new(&config));
                let observed = must(store.read());
                let next = head(if replacement { 2 } else { 1 }, B);
                assert!(!must(store.compare_exchange(observed.as_ref(), &next)));
                let puts = put_calls(root);
                assert_eq!(puts.len(), 1, "a losing write must never be replayed");
                assert_eq!(
                    puts[0]["headers"][if replacement {
                        "if-match"
                    } else {
                        "if-none-match"
                    }],
                    if replacement {
                        json!([ETAG])
                    } else {
                        json!(["*"])
                    }
                );
                check_transport(root, 2.0);
            }
        },
    );
}

#[test]
fn r2_put_http_errors_remain_errors() {
    isolated("r2_put_http_errors_remain_errors", |root| {
        let config = config(root, "r2");
        for status in [400, 401, 403, 404, 429, 500, 503, 301] {
            fixture(root, present(1, A), json!({"put": {"status": status}}));
            let store = must(HeadStore::new(&config));
            let observed = current(&store);
            assert!(
                store
                    .compare_exchange(Some(&observed), &head(2, B))
                    .is_err(),
                "HTTP {status} must fail"
            );
            assert_eq!(put_calls(root).len(), 1);
        }
        fixture(
            root,
            present(1, A),
            json!({"put": {"status": 500, "commit_before_error": true}}),
        );
        let store = must(HeadStore::new(&config));
        let observed = current(&store);
        assert!(
            store
                .compare_exchange(Some(&observed), &head(2, B))
                .is_err()
        );
        assert_eq!(put_calls(root).len(), 1);
    });
}

#[test]
fn r2_lost_put_response_reconciles_the_exact_intended_head() {
    isolated(
        "r2_lost_put_response_reconciles_the_exact_intended_head",
        |root| {
            let config = config(root, "r2");
            for replacement in [false, true] {
                fixture(
                    root,
                    if replacement { present(1, A) } else { absent() },
                    json!({"put": {"status": 0, "exit": 56, "commit_before_error": true}}),
                );
                let store = must(HeadStore::new(&config));
                let observed = must(store.read());
                let revision = if replacement { 2 } else { 1 };
                assert!(must(
                    store.compare_exchange(observed.as_ref(), &head(revision, B))
                ));
                let requests = calls(root);
                let put_index = requests
                    .iter()
                    .position(|call| call["method"] == "PUT")
                    .unwrap();
                assert!(
                    requests[put_index + 1..]
                        .iter()
                        .any(|call| call["method"] == "GET"),
                    "lost responses require a fresh GET"
                );
                assert_eq!(put_calls(root).len(), 1);
                same(&current(&store).head, &head(revision, B));
                check_transport(root, 2.0);
            }
        },
    );
}

#[test]
fn r2_ambiguous_put_with_a_different_current_head_is_false() {
    isolated(
        "r2_ambiguous_put_with_a_different_current_head_is_false",
        |root| {
            let config = config(root, "r2");
            // Same revision/different snapshot and same snapshot/different revision
            // both differ from the complete intended head(2, B).
            for different in [document(2, C), document(3, B), document(1, A)] {
                fixture(
                    root,
                    present(1, A),
                    json!({"put": {"status": 0, "exit": 56, "replace_after": different}}),
                );
                let store = must(HeadStore::new(&config));
                let observed = current(&store);
                assert!(!must(store.compare_exchange(Some(&observed), &head(2, B))));
                assert_eq!(put_calls(root).len(), 1);
                assert_eq!(
                    serde_json::to_value(current(&store).head).unwrap(),
                    different
                );
            }
            fixture(root, absent(), json!({"put": {"status": 0, "exit": 56}}));
            let store = must(HeadStore::new(&config));
            assert!(!must(store.compare_exchange(None, &head(1, B))));
            assert!(must(store.read()).is_none());
            assert_eq!(put_calls(root).len(), 1);
        },
    );
}

#[test]
fn r2_ambiguous_put_with_indeterminate_reread_is_an_error() {
    isolated(
        "r2_ambiguous_put_with_indeterminate_reread_is_an_error",
        |root| {
            let config = config(root, "r2");
            for reread in [
                json!({"status": 503}),
                json!({"status": 0, "exit": 28}),
                json!({"body": "malformed"}),
                json!({"etags": []}),
                json!({"body": document(2, B).to_string().replace(HUB, "wrong-hub")}),
            ] {
                fixture(
                    root,
                    present(1, A),
                    json!({"put": {"status": 0, "exit": 56, "commit_before_error": true}, "after_put_get": reread}),
                );
                let store = must(HeadStore::new(&config));
                let observed = current(&store);
                assert!(
                    store
                        .compare_exchange(Some(&observed), &head(2, B))
                        .is_err()
                );
                assert_eq!(put_calls(root).len(), 1);
            }
        },
    );
}

#[test]
fn r2_rejects_invalid_proposals_before_sending_put() {
    isolated("r2_rejects_invalid_proposals_before_sending_put", |root| {
        let config = config(root, "r2");
        fixture(root, absent(), json!({}));
        let store = must(HeadStore::new(&config));
        for proposal in [head(0, A), head(2, A), head(1, "short")] {
            assert!(store.compare_exchange(None, &proposal).is_err());
        }
        assert!(put_calls(root).is_empty());
        fixture(root, present(1, A), json!({}));
        let observed = current(&store);
        for revision in [0, 1, 3] {
            assert!(
                store
                    .compare_exchange(Some(&observed), &head(revision, B))
                    .is_err()
            );
        }
        assert!(put_calls(root).is_empty());
    });
}

fn sanitized(message: &str) {
    assert!(message.len() <= 16 * 1024, "unbounded diagnostic");
    for secret in [
        ACCESS,
        SECRET,
        SESSION,
        SHARED_PASSWORD,
        BOT_PASSWORD,
        PARENT_SECRET,
    ] {
        assert!(
            !message.contains(secret),
            "diagnostic exposed a synthetic secret"
        );
    }
}

#[test]
fn r2_transport_failures_do_not_expose_credentials_in_diagnostics() {
    isolated(
        "r2_transport_failures_do_not_expose_credentials_in_diagnostics",
        |root| {
            let config = config(root, "r2");
            let diagnostic = format!(
                "AWS_ACCESS_KEY_ID={ACCESS}\nAWS_SECRET_ACCESS_KEY={SECRET}\nAWS_SESSION_TOKEN={SESSION}\npassword={SHARED_PASSWORD}\n{PARENT_SECRET}"
            );
            fixture(
                root,
                present(1, A),
                json!({"get": {"status": 403, "stderr": diagnostic}}),
            );
            let error = HeadStore::new(&config)
                .and_then(|s| s.read())
                .expect_err("403 must fail");
            sanitized(&error.message);
            check_transport(root, 2.0);
            fixture(
                root,
                present(1, A),
                json!({"put": {"status": 500, "stderr": diagnostic}}),
            );
            let store = must(HeadStore::new(&config));
            let observed = current(&store);
            let error = store
                .compare_exchange(Some(&observed), &head(2, B))
                .expect_err("500 must fail");
            sanitized(&error.message);
            check_transport(root, 2.0);
        },
    );
}

#[test]
fn r2_missing_or_empty_signing_credentials_fail_without_transport() {
    isolated(
        "r2_missing_or_empty_signing_credentials_fail_without_transport",
        |root| {
            let config = config(root, "r2");
            for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
                let original = std::env::var_os(name).unwrap();
                for empty in [false, true] {
                    fixture(root, present(1, A), json!({}));
                    // This mutation occurs only in the isolated worker, with
                    // no concurrently running tests or backend operations.
                    unsafe {
                        if empty {
                            std::env::set_var(name, "");
                        } else {
                            std::env::remove_var(name);
                        }
                    }
                    let result = HeadStore::new(&config).and_then(|store| store.read());
                    unsafe { std::env::set_var(name, &original) };
                    let error = result.expect_err("missing signing credentials must fail");
                    sanitized(&error.message);
                    assert!(calls(root).is_empty());
                }
            }
        },
    );
}

#[test]
fn r2_rejects_url_credential_and_etag_injection_before_transport() {
    isolated(
        "r2_rejects_url_credential_and_etag_injection_before_transport",
        |root| {
            let baseline = config(root, "r2");
            for token in ["\"ok\"\r\nX-Injected: yes", "\"bad\0tag\""] {
                fixture(root, present(1, A), json!({}));
                let store = must(HeadStore::new(&baseline));
                let forged = ObservedHead {
                    head: head(1, A),
                    token: token.into(),
                };
                assert!(store.compare_exchange(Some(&forged), &head(2, B)).is_err());
                assert!(put_calls(root).is_empty());
            }
            // Environment is changed only inside this isolated, single-thread test
            // worker; it is restored before the next synthetic credential is tested.
            for name in [
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
            ] {
                let original = std::env::var_os(name).unwrap();
                for injection in ["synthetic\rheader", "synthetic\nheader"] {
                    fixture(root, present(1, A), json!({}));
                    unsafe { std::env::set_var(name, injection) };
                    let result = HeadStore::new(&baseline).and_then(|s| s.read());
                    unsafe { std::env::set_var(name, &original) };
                    assert!(result.is_err());
                    assert!(calls(root).is_empty(), "injected credential reached curl");
                }
            }
            // NUL cannot be represented in a Unix environment variable; URLs and
            // ETags exercise NUL validation at the typed API boundary instead.
            for url in [
                format!("{REPOSITORY}\r\nheader"),
                format!("{REPOSITORY}\0suffix"),
                REPOSITORY.replacen("https://", "http://", 1),
                format!("{REPOSITORY}?query=1"),
                format!("{REPOSITORY}#fragment"),
                REPOSITORY.replacen("https://", "https://user:password@", 1),
            ] {
                fixture(root, present(1, A), json!({}));
                let mut invalid = config(root, "r2");
                invalid.repositories.shared.url = Some(url);
                let result = HeadStore::new(&invalid).and_then(|s| s.read());
                assert!(result.is_err());
                assert!(calls(root).is_empty(), "invalid URL reached curl");
            }
        },
    );
}

#[test]
fn r2_timeout_honors_smaller_config_and_the_120_second_cap() {
    isolated(
        "r2_timeout_honors_smaller_config_and_the_120_second_cap",
        |root| {
            for seconds in [1, 300] {
                let mut config = config(root, "r2");
                config.sync.stuck_request_timeout_seconds = seconds;
                fixture(root, present(1, A), json!({}));
                let store = must(HeadStore::new(&config));
                same(&current(&store).head, &head(1, A));
                check_transport(root, seconds as f64);
            }
            let mut config = config(root, "r2");
            config.sync.stuck_request_timeout_seconds = 1;
            fixture(root, present(1, A), json!({"get": {"delay_seconds": 3}}));
            let start = Instant::now();
            assert!(HeadStore::new(&config).and_then(|s| s.read()).is_err());
            assert!(start.elapsed() < Duration::from_secs(4));
            check_transport(root, 1.0);
        },
    );
}

#[cfg(target_os = "linux")]
fn peak_memory_kib() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn r2_stdout_and_stderr_are_bounded_even_for_malformed_responses() {
    isolated(
        "r2_stdout_and_stderr_are_bounded_even_for_malformed_responses",
        |root| {
            let config = config(root, "r2");
            for stream in ["stdout_bytes", "stderr_bytes"] {
                fixture(
                    root,
                    present(1, A),
                    json!({"get": {"status": 500, (stream): 32 * 1024 * 1024}}),
                );
                #[cfg(target_os = "linux")]
                let before = peak_memory_kib();
                let start = Instant::now();
                let error = HeadStore::new(&config)
                    .and_then(|s| s.read())
                    .expect_err("malformed oversized response must fail");
                sanitized(&error.message);
                assert!(start.elapsed() < Duration::from_secs(8));
                #[cfg(target_os = "linux")]
                assert!(
                    peak_memory_kib() <= before + 16 * 1024,
                    "{stream} was buffered without a bound"
                );
            }
        },
    );
}

// Small-object store over the same offline curl boundary (plan Task 1).

const LIST: &str = "uah-live/list.json";
const LIST_URL: &str = "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/ownerless-head-fixture/shared/prefix/uah-live/list.json";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Remote state with an absent head; other keys are served from `stored`.
fn remote_objects(stored: Value) -> Value {
    json!({"exists": false, "etag": "\"absent\"", "objects": stored})
}

fn object(body: &[u8]) -> Value {
    json!({"etag": ETAG, "body_hex": hex(body)})
}

fn failure<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("live object operation must fail"),
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

#[test]
fn r2_object_get_handles_200_304_404() {
    isolated("r2_object_get_handles_200_304_404", |root| {
        let mut slow = config(root, "r2");
        slow.sync.stuck_request_timeout_seconds = 1;
        let mut capped = config(root, "r2");
        capped.sync.stuck_request_timeout_seconds = 300;
        let config = config(root, "r2");
        let body = b"\x00sealed\r\n\xff";
        let stored = || remote_objects(json!({(LIST): object(body)}));
        let absent = format!(
            "{}/uah-live/absent.json",
            REPOSITORY.strip_prefix("s3:").unwrap()
        );
        fixture(root, stored(), json!({}));
        let store = must(ObjectStore::new(&config, REPOSITORY));
        assert_eq!(
            found(must(store.get(LIST, None, 1024))),
            (body.to_vec(), ETAG.to_owned())
        );
        assert!(matches!(
            must(store.get(LIST, Some(ETAG), 1024)),
            Fetched::NotModified
        ));
        assert_eq!(
            found(must(store.get(LIST, Some("\"stale\""), 1024))),
            (body.to_vec(), ETAG.to_owned())
        );
        assert!(matches!(
            must(store.get("uah-live/absent.json", None, 1024)),
            Fetched::Missing
        ));
        let gets = calls(root);
        let methods: Vec<_> = gets.iter().map(|call| call["method"].as_str()).collect();
        assert_eq!(methods, [Some("GET"); 4]);
        assert!(gets[0]["headers"].get("if-none-match").is_none());
        assert_eq!(gets[1]["headers"]["if-none-match"], json!([ETAG]));
        assert_eq!(gets[2]["headers"]["if-none-match"], json!(["\"stale\""]));
        check_transport_to(root, 2.0, &[LIST_URL, absent.as_str()]);

        for status in [201, 204, 206, 301, 401, 403, 412, 429, 500, 503] {
            fixture(root, stored(), json!({"get": {"status": status}}));
            assert_eq!(
                failure(store.get(LIST, None, 1024)),
                format!("Live object GET failed (HTTP {status}); check backend access and retry")
            );
        }
        for broken in [
            json!({"status": 0, "exit": 7}),
            json!({"status": 200, "exit": 56}),
        ] {
            fixture(root, stored(), json!({ "get": broken }));
            assert_eq!(
                failure(store.get(LIST, None, 1024)),
                "Live object GET transport failed; retry the read"
            );
        }
        fixture(root, stored(), json!({"get": {"delay_seconds": 3}}));
        let started = Instant::now();
        assert_eq!(
            failure(must(ObjectStore::new(&slow, REPOSITORY)).get(LIST, None, 1024)),
            "Live object GET transport failed; retry the read"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        check_transport_to(root, 1.0, &[LIST_URL]);
        fixture(root, stored(), json!({}));
        must(must(ObjectStore::new(&capped, REPOSITORY)).get(LIST, None, 1024));
        check_transport_to(root, 300.0, &[LIST_URL]); // Asserts the 120-second cap.
        for etags in [
            json!([]),
            json!(["W/\"weak\""]),
            json!(["unquoted"]),
            json!([ETAG, "\"other\""]),
        ] {
            fixture(root, stored(), json!({"get": {"etags": etags}}));
            assert!(
                store.get(LIST, None, 1024).is_err(),
                "HTTP 200 accepted with ETags {etags}"
            );
        }
        for token in ["", "unquoted", "W/\"weak\"", "\"ok\"\r\nX-Injected: yes"] {
            fixture(root, stored(), json!({}));
            assert!(
                store.get(LIST, Some(token), 1024).is_err(),
                "sent If-None-Match {token:?}"
            );
            assert!(calls(root).is_empty(), "an invalid token reached curl");
        }
    });
}

#[test]
fn r2_object_put_sends_conditions_and_maps_412() {
    isolated("r2_object_put_sends_conditions_and_maps_412", |root| {
        let config = config(root, "r2");
        let body = b"\x00sealed\r\n\xff";
        fixture(root, remote_objects(json!({})), json!({}));
        let store = must(ObjectStore::new(&config, REPOSITORY));
        let created = "\"new.etag/+==:2\""; // The fake's ETag for its first write.
        assert_eq!(must(store.put(LIST, body, Condition::Create)), Written::Yes);
        assert_eq!(
            must(store.put(LIST, b"second", Condition::Create)),
            Written::Conflict
        );
        assert_eq!(
            must(store.put(LIST, b"replaced", Condition::Match(created))),
            Written::Yes
        );
        assert_eq!(
            must(store.put(LIST, b"stale", Condition::Match(created))),
            Written::Conflict
        );
        let puts = put_calls(root);
        assert_eq!(puts.len(), 4, "a losing write must never be replayed");
        for (call, (header, value)) in puts.iter().zip([
            ("if-none-match", "*"),
            ("if-none-match", "*"),
            ("if-match", created),
            ("if-match", created),
        ]) {
            assert_eq!(call["headers"][header], json!([value]));
        }
        assert_eq!(puts[0]["body_hex"], hex(body));
        assert_eq!(puts[2]["body_hex"], hex(b"replaced"));
        assert_eq!(
            found(must(store.get(LIST, None, 1024))),
            (b"replaced".to_vec(), "\"new.etag/+==:3\"".to_owned())
        );
        check_transport_to(root, 2.0, &[LIST_URL]);

        fixture(
            root,
            remote_objects(json!({(LIST): object(b"old")})),
            json!({}),
        );
        assert_eq!(
            must(store.put(LIST, body, Condition::Match(ETAG))),
            Written::Yes
        );
        assert_eq!(put_calls(root)[0]["headers"]["if-match"], json!([ETAG]));
        for (status, written) in [
            (200, Written::Yes),
            (201, Written::Yes),
            (204, Written::Yes),
            (412, Written::Conflict),
        ] {
            fixture(
                root,
                remote_objects(json!({})),
                json!({"put": {"status": status}}),
            );
            assert_eq!(
                must(store.put(LIST, body, Condition::Create)),
                written,
                "HTTP {status}"
            );
            assert_eq!(put_calls(root).len(), 1);
        }
        for status in [301, 400, 401, 403, 404, 409, 429, 500, 503] {
            fixture(
                root,
                remote_objects(json!({})),
                json!({"put": {"status": status}}),
            );
            assert!(
                store.put(LIST, body, Condition::Create).is_err(),
                "HTTP {status} must fail"
            );
            assert_eq!(put_calls(root).len(), 1, "HTTP {status} was replayed");
        }
        fixture(root, remote_objects(json!({})), json!({}));
        let injected = Condition::Match("\"ok\"\r\nX-Injected: yes");
        assert!(store.put(LIST, body, injected).is_err());
        assert!(calls(root).is_empty(), "an injected condition reached curl");
    });
}

#[test]
fn r2_object_put_without_response_is_uncertain() {
    isolated("r2_object_put_without_response_is_uncertain", |root| {
        let mut slow = config(root, "r2");
        slow.sync.stuck_request_timeout_seconds = 1;
        let config = config(root, "r2");
        for (existing, lost) in [
            (
                false,
                json!({"status": 0, "exit": 56, "commit_before_error": true}),
            ),
            (
                true,
                json!({"status": 0, "exit": 56, "commit_before_error": true}),
            ),
            (false, json!({"status": 0, "exit": 7})),
            (true, json!({"status": 200, "exit": 56})),
        ] {
            let stored = if existing {
                json!({(LIST): object(b"old")})
            } else {
                json!({})
            };
            fixture(root, remote_objects(stored), json!({ "put": lost }));
            let store = must(ObjectStore::new(&config, REPOSITORY));
            let condition = if existing {
                Condition::Match(ETAG)
            } else {
                Condition::Create
            };
            assert_eq!(
                must(store.put(LIST, b"new", condition)),
                Written::Uncertain,
                "{lost}"
            );
            assert_eq!(put_calls(root).len(), 1, "an uncertain PUT was replayed");
            check_transport_to(root, 2.0, &[LIST_URL]);
        }
        fixture(
            root,
            remote_objects(json!({})),
            json!({"put": {"delay_seconds": 3}}),
        );
        let started = Instant::now();
        assert_eq!(
            must(must(ObjectStore::new(&slow, REPOSITORY)).put(LIST, b"new", Condition::Create)),
            Written::Uncertain
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(put_calls(root).len(), 1);
    });
}

#[test]
fn r2_object_delete_accepts_404() {
    isolated("r2_object_delete_accepts_404", |root| {
        let config = config(root, "r2");
        let stored = || remote_objects(json!({(LIST): object(b"old")}));
        fixture(root, stored(), json!({}));
        let store = must(ObjectStore::new(&config, REPOSITORY));
        must(store.delete(LIST));
        assert!(matches!(
            must(store.get(LIST, None, 1024)),
            Fetched::Missing
        ));
        must(store.delete(LIST));
        let deletes: Vec<_> = calls(root)
            .iter()
            .filter(|call| call["method"] == "DELETE")
            .map(|call| call["status"].as_u64())
            .collect();
        assert_eq!(deletes, [Some(204), Some(404)]);
        check_transport_to(root, 2.0, &[LIST_URL]);
        for status in [200, 204, 404] {
            fixture(root, stored(), json!({"delete": {"status": status}}));
            must(store.delete(LIST));
        }
        for failed in [
            json!({"status": 403}),
            json!({"status": 412}),
            json!({"status": 500}),
            json!({"status": 0, "exit": 7}),
            json!({"status": 204, "exit": 56}),
        ] {
            fixture(root, stored(), json!({ "delete": failed }));
            assert!(store.delete(LIST).is_err(), "DELETE {failed} must fail");
        }
    });
}

#[test]
fn r2_object_url_is_prefix_plus_key() {
    isolated("r2_object_url_is_prefix_plus_key", |root| {
        let config = config(root, "r2");
        for repository in [REPOSITORY.to_owned(), format!("{REPOSITORY}/")] {
            fixture(root, remote_objects(json!({})), json!({}));
            let store = must(ObjectStore::new(&config, &repository));
            assert!(matches!(
                must(store.get(LIST, None, 1024)),
                Fetched::Missing
            ));
            assert_eq!(must(store.put(LIST, b"x", Condition::Create)), Written::Yes);
            must(store.delete(LIST));
            let requests = calls(root);
            let methods: Vec<_> = requests
                .iter()
                .map(|call| call["method"].as_str())
                .collect();
            assert_eq!(methods, [Some("GET"), Some("PUT"), Some("DELETE")]);
            for call in &requests {
                assert_eq!(option(call, "url"), Some(LIST_URL), "from {repository}");
            }
            check_transport_to(root, 2.0, &[LIST_URL]);
        }
        for repository in [
            REPOSITORY.replacen("https://", "http://", 1),
            format!("{REPOSITORY}?query=1"),
            REPOSITORY.replacen("https://", "https://user:password@", 1),
            "s3:https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com".to_owned(),
            format!("{REPOSITORY}/%2e%2e"),
        ] {
            fixture(root, remote_objects(json!({})), json!({}));
            assert!(
                ObjectStore::new(&config, &repository)
                    .and_then(|store| store.get(LIST, None, 1024))
                    .is_err(),
                "accepted {repository}"
            );
            assert!(calls(root).is_empty(), "an invalid URL reached curl");
        }
        let store = must(ObjectStore::new(&config, REPOSITORY));
        for key in [
            "",
            "../x",
            "a//b",
            "a/./b",
            "a b",
            "a/b/c/d/e/f/g/h/i",
            "/uah-live/list.json",
            "a%2Fb",
            "a?b",
            "a#b",
        ] {
            fixture(root, remote_objects(json!({})), json!({}));
            let invalid = "Invalid live object key";
            assert_eq!(failure(store.get(key, None, 1024)), invalid, "{key:?}");
            assert_eq!(
                failure(store.put(key, b"x", Condition::Create)),
                invalid,
                "{key:?}"
            );
            assert_eq!(failure(store.delete(key)), invalid, "{key:?}");
            assert!(calls(root).is_empty(), "an invalid key reached curl");
        }
    });
}

#[test]
fn r2_object_bodies_follow_the_limit_up_to_one_mebibyte() {
    isolated(
        "r2_object_bodies_follow_the_limit_up_to_one_mebibyte",
        |root| {
            let config = config(root, "r2");
            let largest = vec![b'z'; MAX_OBJECT_BYTES as usize];
            let oversized = [largest.as_slice(), &b"z"[..]].concat();
            fixture(
                root,
                remote_objects(json!({(LIST): object(&largest)})),
                json!({}),
            );
            let store = must(ObjectStore::new(&config, REPOSITORY));
            let (body, token) = found(must(store.get(LIST, None, MAX_OBJECT_BYTES)));
            assert!(
                body == largest,
                "a MAX_OBJECT_BYTES body must arrive intact"
            );
            assert_eq!(token, ETAG);
            check_transport_to(root, 2.0, &[LIST_URL]);
            assert!(store.get(LIST, None, MAX_OBJECT_BYTES - 1).is_err());
            assert!(store.get(LIST, None, MAX_OBJECT_BYTES + 1).is_err());

            fixture(root, remote_objects(json!({})), json!({}));
            assert!(store.put(LIST, &oversized, Condition::Create).is_err());
            assert!(put_calls(root).is_empty(), "an oversized body was uploaded");
            assert_eq!(
                must(store.put(LIST, &largest, Condition::Create)),
                Written::Yes
            );
            let puts = put_calls(root);
            assert_eq!(puts.len(), 1);
            assert_eq!(puts[0]["body_size"], MAX_OBJECT_BYTES);
            assert_eq!(
                puts[0]["body_sha256"],
                format!("{:x}", Sha256::digest(&largest))
            );
            check_transport_to(root, 2.0, &[LIST_URL]);
        },
    );
}
