#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use std::fs::{self, File, FileTimes};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn readonly_tree(root: &Path, prefix: &str) -> Vec<PathBuf> {
    write(
        root.join(prefix).join("nested/payload.md"),
        vec![b'r'; 64 * 1024],
    );
    fs::create_dir_all(root.join(prefix).join("empty")).unwrap();
    fs::create_dir_all(root.join(prefix).join("nested/empty")).unwrap();
    let paths: Vec<_> = ["", "nested", "empty", "nested/empty"]
        .map(|name| PathBuf::from(prefix).join(name))
        .into();
    set_readonly_metadata(root, &paths);
    paths
}

fn modified() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_055_055)
}

fn set_readonly_metadata(root: &Path, paths: &[PathBuf]) {
    for relative in paths {
        let path = root.join(relative);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o550)).unwrap();
        File::open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified()))
            .unwrap();
    }
}

fn assert_readonly_metadata(root: &Path, paths: &[PathBuf]) {
    let differences: Vec<_> = paths
        .iter()
        .filter_map(|relative| {
            let actual = fs::metadata(root.join(relative)).unwrap();
            let mode = actual.permissions().mode() & 0o7777;
            let mtime = actual.modified().unwrap();
            (mode != 0o550 || mtime != modified()).then(|| {
                format!(
                    "{}: expected 0550 and {:?}, got {mode:04o} and {mtime:?}",
                    relative.display(),
                    modified()
                )
            })
        })
        .collect();
    assert!(
        differences.is_empty(),
        "0550 directory metadata changed:\n{}",
        differences.join("\n")
    );
}

fn restore(fixture: &Fixture, repository: &str, snapshot: &str) -> PathBuf {
    let destination = fixture
        .temp
        .path()
        .join(format!("readonly-{repository}-recovery"));
    fixture.ok(
        &fixture.alpha,
        &[
            "restore",
            "--repository",
            repository,
            "--snapshot",
            snapshot,
            "--destination",
            destination.to_str().unwrap(),
        ],
    );
    destination
}

#[test]
fn backup_restore_preserves_0550_directories_and_their_mtimes() {
    let fixture = Fixture::new();
    let owned = fixture.alpha.path("owned");
    let shared = fixture.alpha.path("shared");
    let owned_paths = readonly_tree(&owned, "notes");
    let shared_paths = readonly_tree(&shared, "readonly");
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    fixture.ok(&fixture.alpha, &["sync"]);
    assert_readonly_metadata(&owned, &owned_paths);
    assert_readonly_metadata(&shared, &shared_paths);
    let bot_id = fixture
        .content_snapshots(&fixture.alpha, "bot")
        .last()
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let bot_restore = restore(&fixture, "bot", &bot_id);
    let shared_restore = restore(
        &fixture,
        "shared",
        &fixture.current_shared_id(&fixture.alpha),
    );
    for (root, prefix) in [(&bot_restore, "notes"), (&shared_restore, "readonly")] {
        assert_eq!(
            fs::read(root.join(prefix).join("nested/payload.md")).unwrap(),
            vec![b'r'; 64 * 1024]
        );
        assert!(root.join(prefix).join("empty").is_dir());
        assert!(root.join(prefix).join("nested/empty").is_dir());
    }
    // Both repositories are restored before comparing metadata, so either path can expose loss.
    let mut mismatches = Vec::new();
    for (root, paths) in [
        (&bot_restore, &owned_paths),
        (&shared_restore, &shared_paths),
    ] {
        for relative in paths {
            let actual = fs::metadata(root.join(relative)).unwrap();
            if actual.permissions().mode() & 0o7777 != 0o550
                || actual.modified().unwrap() != modified()
            {
                mismatches.push(format!(
                    "{}: mode {:04o}, mtime {:?}",
                    root.join(relative).display(),
                    actual.permissions().mode() & 0o7777,
                    actual.modified().unwrap()
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "restored directories must retain original mode 0550 and mtime {:?}:\n{}",
        modified(),
        mismatches.join("\n")
    );
}

#[test]
fn repeated_sync_and_shared_cache_cleanup_preserve_0550_source_directories() {
    let fixture = Fixture::new();
    let owned = fixture.alpha.path("owned");
    let shared = fixture.alpha.path("shared");
    let owned_paths = readonly_tree(&owned, "notes");
    let shared_paths = readonly_tree(&shared, "readonly");
    fixture.ok(&fixture.alpha, &["setup"]);
    for _ in 0..3 {
        fixture.ok(&fixture.alpha, &["sync"]);
        assert_readonly_metadata(&owned, &owned_paths);
        assert_readonly_metadata(&shared, &shared_paths);
    }
    let first = fixture.active_shared(&fixture.alpha);
    let root = first.parent().unwrap().parent().unwrap().to_path_buf();
    let mut publisher = Bot {
        file: fixture.temp.path().join("readonly-publisher.yaml"),
        config: fixture.alpha.config.clone(),
    };
    publisher.config["paths"]["shared"] = json!(fixture.temp.path().join("publisher-shared"));
    publisher.config["paths"]["imports"] = json!(fixture.temp.path().join("publisher-imports"));
    publisher.config["paths"]["state"] = json!(fixture.temp.path().join("publisher-state"));
    copy_tree(&first, &publisher.path("shared"));
    fs::remove_file(publisher.path("shared").join("registry-history.json")).unwrap();
    set_readonly_metadata(&publisher.path("shared"), &shared_paths);
    publisher.save();
    let mut generations = vec![first];
    for generation in 1..=3 {
        write(
            publisher.path("shared").join("generation.md"),
            format!("{generation}"),
        );
        fixture.ok(&publisher, &["registry", "publish"]);
        fixture.ok(&fixture.alpha, &["sync"]);
        let current = fixture.active_shared(&fixture.alpha);
        assert_eq!(
            current.file_name().unwrap().to_str().unwrap(),
            fixture.current_shared_id(&publisher)
        );
        assert_eq!(
            fs::read(current.join("generation.md")).unwrap(),
            generation.to_string().as_bytes()
        );
        assert_eq!(
            fs::read(current.join("readonly/nested/payload.md")).unwrap(),
            vec![b'r'; 64 * 1024]
        );
        assert_readonly_metadata(&owned, &owned_paths);
        assert_readonly_metadata(&shared, &shared_paths);
        assert_readonly_metadata(&publisher.path("shared"), &shared_paths);
        generations.push(current);
        for expired in generations.iter().rev().skip(2) {
            assert!(
                !expired.exists(),
                "cleanup must remove obsolete cached trees containing original 0550 directories: {}",
                expired.display()
            );
        }
        assert_eq!(fs::read_dir(root.join("versions")).unwrap().count(), 2);
        let pointer: Value =
            serde_json::from_slice(&fs::read(root.join("CURRENT")).unwrap()).unwrap();
        assert_eq!(
            pointer["previous"],
            generations[generations.len() - 2]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        );
    }
}
