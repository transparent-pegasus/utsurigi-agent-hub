#[allow(dead_code)]
mod common;

use common::*;
use serde_json::json;
use std::fs::{self, File, FileTimes};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

fn backup(fixture: &Fixture) -> String {
    let before = fixture.ids(&fixture.alpha, "bot");
    fixture.ok(&fixture.alpha, &["sync"]);
    let after = fixture.ids(&fixture.alpha, "bot");
    let added: Vec<_> = after.difference(&before).cloned().collect();
    assert_eq!(
        added.len(),
        1,
        "sync must create one recoverable Bot backup"
    );
    added[0].clone()
}

fn recover(fixture: &Fixture, repository: &str, snapshot: &str, name: &str) -> PathBuf {
    let destination = fixture.temp.path().join(name);
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

#[cfg(target_os = "linux")]
fn io_writes(path: &Path) -> u64 {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("wchar: "))
        .expect("Linux /proc process I/O must expose wchar")
        .parse()
        .unwrap()
}

#[cfg(target_os = "linux")]
fn unchanged_sync_io(shared: bool) {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let payload = vec![if shared { b's' } else { b'o' }; 4 * 1024 * 1024];
    let relative = if shared { "large.md" } else { "notes/large.md" };
    let source = fixture
        .alpha
        .path(if shared { "shared" } else { "owned" })
        .join(relative);
    write(&source, &payload);
    fixture.ok(&fixture.alpha, &["setup"]);
    backup(&fixture);
    backup(&fixture);
    let head = fixture.shared_head(&fixture.alpha);
    let active = fixture.active_shared(&fixture.alpha);
    fs::write(tool.join("mode"), "capture-io").unwrap();
    let mut writes = Vec::new();
    let mut last = String::new();
    for _ in 0..2 {
        last = backup(&fixture);
        writes.push(io_writes(&tool.join("hub-io-end")) - io_writes(&tool.join("hub-io-start")));
        assert_eq!(fixture.shared_head(&fixture.alpha), head);
        assert_eq!(fixture.active_shared(&fixture.alpha), active);
        assert_eq!(fs::read(&source).unwrap(), payload);
    }
    if shared {
        assert_eq!(fs::read(active.join(relative)).unwrap(), payload);
    } else {
        let restored = recover(&fixture, "bot", &last, "io-recovery");
        assert_eq!(fs::read(restored.join(relative)).unwrap(), payload);
    }
    // Count hub writes, excluding restic's encrypted repository traffic and validation reads.
    assert!(
        writes.iter().all(|bytes| *bytes < payload.len() as u64 / 4),
        "unchanged {} sync rewrote {writes:?} bytes for a {}-byte source; full content copies must be avoided",
        if shared {
            "accepted shared cache"
        } else {
            "owned"
        },
        payload.len()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unchanged_sync_avoids_full_owned_content_copies() {
    unchanged_sync_io(false);
}

#[cfg(target_os = "linux")]
#[test]
fn unchanged_sync_avoids_full_accepted_shared_content_copies() {
    unchanged_sync_io(true);
}

fn set_metadata(path: &Path, mode: u32, modified: SystemTime) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    File::open(path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
}

fn metadata_tree(root: &Path, prefix: &str) -> Vec<(PathBuf, u32, SystemTime)> {
    for directory in ["", "/nested", "/empty", "/nested/empty"] {
        fs::create_dir_all(root.join(format!("{prefix}{directory}"))).unwrap();
    }
    let entries = [
        ("executable", 0o751),
        ("group", 0o640),
        ("private", 0o600),
        ("nested/private", 0o600),
    ];
    let mut expected = Vec::new();
    for (index, (name, mode)) in entries.into_iter().enumerate() {
        let relative = PathBuf::from(prefix).join(name);
        write(
            root.join(&relative),
            format!("recoverable {prefix}/{name}\n"),
        );
        expected.push((
            relative,
            mode,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000 + index as u64),
        ));
    }
    for (index, name) in ["nested/empty", "nested", "empty", ""]
        .into_iter()
        .enumerate()
    {
        expected.push((
            PathBuf::from(prefix).join(name),
            0o750,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_100 + index as u64),
        ));
    }
    for (relative, mode, modified) in &expected {
        set_metadata(&root.join(relative), *mode, *modified);
    }
    expected
}

fn assert_metadata(root: &Path, expected: &[(PathBuf, u32, SystemTime)]) {
    let mut differences = Vec::new();
    for (relative, mode, modified) in expected {
        let path = root.join(relative);
        let actual =
            fs::metadata(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let actual_mode = actual.permissions().mode() & 0o7777;
        if actual_mode != *mode || actual.modified().unwrap() != *modified {
            differences.push(format!(
                "{}: expected mode {mode:04o}, mtime {modified:?}; got {actual_mode:04o}, {:?}",
                relative.display(),
                actual.modified().unwrap()
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "metadata was not preserved:\n{}",
        differences.join("\n")
    );
}

fn metadata_round_trip(shared: bool) {
    let fixture = Fixture::new();
    let source = fixture.alpha.path(if shared { "shared" } else { "owned" });
    let prefix = if shared { "published" } else { "notes" };
    let expected = metadata_tree(&source, prefix);
    fixture.ok(&fixture.alpha, &["setup"]);
    let bot_snapshot = backup(&fixture);
    assert_metadata(&source, &expected);
    let repository = if shared { "shared" } else { "bot" };
    let snapshot = if shared {
        fixture.current_shared_id(&fixture.alpha)
    } else {
        bot_snapshot
    };
    let restored = recover(&fixture, repository, &snapshot, "metadata-recovery");
    for (relative, _, _) in &expected {
        if source.join(relative).is_file() {
            assert_eq!(
                fs::read(restored.join(relative)).unwrap(),
                fs::read(source.join(relative)).unwrap()
            );
        } else {
            assert!(restored.join(relative).is_dir());
        }
    }
    for root in [
        fixture.alpha.path("state"),
        fixture.alpha.path("imports"),
        restored.clone(),
    ] {
        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o077,
            0,
            "wrapper directories must remain private"
        );
    }
    assert_metadata(&restored, &expected);
}

#[test]
fn backup_restore_preserves_owned_file_and_directory_modes_and_mtimes() {
    metadata_round_trip(false);
}

#[test]
fn backup_restore_preserves_shared_file_and_directory_modes_and_mtimes() {
    metadata_round_trip(true);
}

#[test]
fn owned_capture_does_not_widen_private_files_before_backup() {
    let mut fixture = Fixture::new();
    let source = fixture.alpha.path("owned").join("notes/private");
    write(&source, b"private source");
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    fixture.ok(&fixture.alpha, &["setup"]);
    fs::write(tool.join("private-relative"), "notes/private").unwrap();
    fs::write(tool.join("mode"), "audit-private").unwrap();
    let output = fixture.cli(&fixture.alpha, &["sync"]);
    assert_eq!(
        fs::metadata(&source).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read(&source).unwrap(), b"private source");
    success(&output);
    assert!(tool.join("private-capture-checked").is_file());
}

#[test]
fn capture_refreshes_changed_bytes_even_with_unchanged_size_and_mtime() {
    let fixture = Fixture::new();
    let source = fixture.alpha.path("owned").join("notes/change.md");
    write(&source, b"before");
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    fixture.ok(&fixture.alpha, &["setup"]);
    let first = backup(&fixture);
    fs::write(&source, b"after!").unwrap();
    File::open(&source)
        .unwrap()
        .set_times(FileTimes::new().set_modified(modified))
        .unwrap();
    let second = backup(&fixture);
    for (snapshot, name, bytes) in [
        (&first, "old-content", b"before"),
        (&second, "new-content", b"after!"),
    ] {
        let restored = recover(&fixture, "bot", snapshot, name);
        assert_eq!(fs::read(restored.join("notes/change.md")).unwrap(), bytes);
    }
    assert_eq!(fs::read(source).unwrap(), b"after!");
}

#[test]
fn capture_refreshes_owned_file_directory_type_replacements() {
    let fixture = Fixture::new();
    let source = fixture.alpha.path("owned").join("notes/item");
    write(&source, b"original file");
    fixture.ok(&fixture.alpha, &["setup"]);
    let first = backup(&fixture);

    fs::remove_file(&source).unwrap();
    write(source.join("child"), b"directory child");
    let second = backup(&fixture);

    fs::remove_dir_all(&source).unwrap();
    write(&source, b"replacement file");
    let third = backup(&fixture);

    let original = recover(&fixture, "bot", &first, "type-original-file");
    assert!(original.join("notes/item").is_file());
    assert_eq!(
        fs::read(original.join("notes/item")).unwrap(),
        b"original file"
    );
    let directory = recover(&fixture, "bot", &second, "type-directory");
    assert!(directory.join("notes/item").is_dir());
    assert_eq!(
        fs::read(directory.join("notes/item/child")).unwrap(),
        b"directory child"
    );
    let replacement = recover(&fixture, "bot", &third, "type-replacement-file");
    assert!(replacement.join("notes/item").is_file());
    assert_eq!(
        fs::read(replacement.join("notes/item")).unwrap(),
        b"replacement file"
    );
    assert_eq!(fs::read(source).unwrap(), b"replacement file");
}

#[test]
fn capture_refreshes_file_and_directory_metadata_changes() {
    let fixture = Fixture::new();
    let owned = fixture.alpha.path("owned");
    write(owned.join("notes/private"), b"private");
    fixture.ok(&fixture.alpha, &["setup"]);
    backup(&fixture);
    let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_123_456);
    let expected = vec![
        (PathBuf::from("notes/private"), 0o600, modified),
        (PathBuf::from("notes"), 0o750, modified),
    ];
    for (relative, mode, mtime) in &expected {
        set_metadata(&owned.join(relative), *mode, *mtime);
    }
    let snapshot = backup(&fixture);
    let restored = recover(&fixture, "bot", &snapshot, "changed-metadata");
    assert_metadata(&owned, &expected);
    assert_metadata(&restored, &expected);
}

#[test]
fn capture_refreshes_changed_selection_exclusions_and_source_root() {
    let mut fixture = Fixture::new();
    let owned = fixture.alpha.path("owned");
    write(owned.join("other/selected.md"), b"second selection");
    write(owned.join("other/excluded.md"), b"must stay local");
    fixture.ok(&fixture.alpha, &["setup"]);
    backup(&fixture);
    fixture.alpha.config["data"]["include"] = json!(["other"]);
    fixture.alpha.config["data"]["exclude"] = json!(["other/excluded.md"]);
    fixture.alpha.save();
    let snapshot = backup(&fixture);
    let restored = recover(&fixture, "bot", &snapshot, "changed-selection");
    assert_eq!(
        fs::read(restored.join("other/selected.md")).unwrap(),
        b"second selection"
    );
    assert!(!restored.join("notes").exists());
    assert!(!restored.join("other/excluded.md").exists());
    let alternate = fixture.temp.path().join("alternate-owned");
    write(alternate.join("other/selected.md"), b"alternate source");
    fixture.alpha.config["paths"]["owned"] = json!(alternate);
    fixture.alpha.save();
    let snapshot = backup(&fixture);
    let restored = recover(&fixture, "bot", &snapshot, "changed-source-root");
    assert_eq!(
        fs::read(restored.join("other/selected.md")).unwrap(),
        b"alternate source"
    );
    assert_eq!(
        fs::read(owned.join("other/selected.md")).unwrap(),
        b"second selection"
    );
    assert_eq!(
        fs::read(owned.join("other/excluded.md")).unwrap(),
        b"must stay local"
    );
}

#[test]
fn cached_capture_still_rejects_selected_symlinks() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let owned = fixture.alpha.path("owned");
    let source = owned.join("notes/selected.md");
    write(&source, b"selected");
    fixture.ok(&fixture.alpha, &["setup"]);
    backup(&fixture);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    fs::remove_file(&source).unwrap();
    write(owned.join("target.md"), b"must remain local");
    symlink(owned.join("target.md"), &source).unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    failure(&fixture.cli(&fixture.alpha, &["sync"]));
    assert_no_writes(&tool);
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    assert_eq!(
        fs::read(owned.join("target.md")).unwrap(),
        b"must remain local"
    );
}

#[test]
fn cached_capture_still_validates_sqlite_declarations_and_sidecars() {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let owned = fixture.alpha.path("owned");
    write(
        owned.join("notes/export.sqlite"),
        include_bytes!("fixtures/quiescent.sqlite"),
    );
    fixture.alpha.config["data"]["sqlite_snapshots"] = json!(["notes/export.sqlite"]);
    fixture.alpha.save();
    fixture.ok(&fixture.alpha, &["setup"]);
    backup(&fixture);
    let snapshots = fixture.ids(&fixture.alpha, "bot");
    fixture.alpha.config["data"]["sqlite_snapshots"] = json!([]);
    fixture.alpha.save();
    fs::write(tool.join("calls"), "").unwrap();
    let output = fixture.cli(&fixture.alpha, &["sync"]);
    failure(&output);
    assert!(text(&output).contains("SQLite"), "{}", text(&output));
    assert_no_writes(&tool);
    fixture.alpha.config["data"]["sqlite_snapshots"] = json!(["notes/export.sqlite"]);
    fixture.alpha.save();
    for suffix in ["-wal", "-shm"] {
        let sidecar = owned.join(format!("notes/export.sqlite{suffix}"));
        write(&sidecar, b"unsettled sidecar");
        fs::write(tool.join("calls"), "").unwrap();
        let output = fixture.cli(&fixture.alpha, &["sync"]);
        failure(&output);
        assert!(text(&output).contains("sidecar"), "{}", text(&output));
        assert_no_writes(&tool);
        fs::remove_file(sidecar).unwrap();
    }
    assert_eq!(fixture.ids(&fixture.alpha, "bot"), snapshots);
    assert_eq!(
        fs::read(owned.join("notes/export.sqlite")).unwrap(),
        include_bytes!("fixtures/quiescent.sqlite")
    );
}
