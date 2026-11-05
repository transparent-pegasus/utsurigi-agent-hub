use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const HASH: &str = "char-ngram-hash-1024";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = loop {
            let root = std::env::temp_dir().join(format!(
                "utsurigi-memory-cli-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&root) {
                Ok(()) => break root,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create sandbox: {e}"),
            }
        };
        for name in ["home", "data", "cache", "state", "runtime", "package", "db"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        Self { root }
    }

    fn db(&self) -> PathBuf {
        self.root.join("db/memory.db")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_utsurigi-memory"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("TMPDIR", &self.root)
            .env("UTSURIGI_MEMORY_HOME", self.root.join("package"))
            .env("UTSURIGI_MEMORY_PATH", self.db())
            .env("UTSURIGI_MEMORY_CACHE", self.root.join("cache"))
            .env("UTSURIGI_MEMORY_SOCK", self.root.join("sock"))
            .env("UTSURIGI_MEMORY_IDLE_SECS", "2")
            .env("UTSURIGI_MEMORY_NO_DAEMON", "1")
            .current_dir(&self.root);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        run(self.command(args))
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        success(&output);
        serde_json::from_slice(&output.stdout).expect("CLI JSON")
    }

    fn connection(&self) -> Connection {
        Connection::open(self.db()).unwrap()
    }

    fn legacy(&self, embedder: &str) {
        let conn = self.connection();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE memories(
                 id INTEGER PRIMARY KEY AUTOINCREMENT, text TEXT NOT NULL, tag TEXT,
                 created_at INTEGER NOT NULL, vec BLOB NOT NULL);",
        )
        .unwrap();
        conn.execute("INSERT INTO meta VALUES('embedder', ?1)", [embedder])
            .unwrap();
        let mut vector = vec![0u8; if embedder == HASH { 1024 } else { 384 } * 4];
        vector[..4].copy_from_slice(&1f32.to_le_bytes());
        conn.execute(
            "INSERT INTO memories VALUES(41, ?1, 'legacy', 1700000000, ?2)",
            params!["古い記録 🦀", vector],
        )
        .unwrap();
    }

    fn model_failure(&self, output: &Output, before: Option<&[u8]>) {
        assert!(
            !self.root.join("sock").exists(),
            "model failure left a socket: {}",
            diagnostic(output)
        );
        match before {
            Some(bytes) => assert!(
                fs::read(self.db()).unwrap() == bytes,
                "model failure modified an existing database: {}",
                diagnostic(output)
            ),
            None => assert!(
                !self.db().exists(),
                "model failure created a database: {}",
                diagnostic(output)
            ),
        }
        rejected(output);
    }

    fn daemon_attempted(&self) {
        for name in ["utsurigi-memory.lock", "utsurigi-memory.log"] {
            assert!(
                self.root.join("cache").join(name).is_file(),
                "daemon attempt must create {name}"
            );
        }
    }

    fn model(&self, layout: &str) {
        let dir = self.root.join("package/model/multilingual-e5-small");
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            fs::copy(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("model/multilingual-e5-small")
                    .join(name),
                dir.join(name),
            )
            .unwrap();
        }
        let bytes = b"invalid model bytes";
        let digest = if layout == "invalid" {
            format!("{:x}", Sha256::digest(bytes))
        } else {
            format!("{:x}", Sha256::digest(b"expected model bytes"))
        };
        fs::write(
            dir.join("model.onnx.sha256"),
            format!("{digest}  model.onnx\n"),
        )
        .unwrap();
        if layout == "direct" || layout == "invalid" {
            fs::write(dir.join("model.onnx"), bytes).unwrap();
        } else {
            fs::write(dir.join("model.onnx.part00"), &bytes[..8]).unwrap();
            fs::write(dir.join("model.onnx.part01"), &bytes[8..]).unwrap();
            if layout == "cached" {
                fs::write(self.root.join("cache/model.onnx"), bytes).unwrap();
            }
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let socket = self.root.join("sock");
        if let Ok(mut stream) = UnixStream::connect(&socket) {
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = stream.write_all(b"{\"op\":\"stop\"}\n");
            let deadline = Instant::now() + Duration::from_secs(4);
            while socket.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run(mut command: Command) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn utsurigi-memory");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut timed_out = false;
    while child.try_wait().expect("poll utsurigi-memory").is_none() {
        if Instant::now() >= deadline {
            child.kill().expect("kill timed out utsurigi-memory");
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("reap utsurigi-memory");
    assert!(
        !timed_out,
        "CLI exceeded 15 seconds: {}",
        diagnostic(&output)
    );
    output
}

fn diagnostic(output: &Output) -> String {
    format!(
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", diagnostic(output));
}

fn rejected(output: &Output) {
    assert!(!output.status.success(), "{}", diagnostic(output));
    assert!(!output.stderr.is_empty(), "failure needs a diagnostic");
}

fn read_only_rejects_missing_and_uninitialized_database(args: &[&str]) {
    for layout in ["missing-file", "missing-parent", "uninitialized"] {
        let env = Sandbox::new();
        let path = if layout == "missing-parent" {
            env.root.join("missing/nested/memory.db")
        } else {
            env.db()
        };
        let before = if layout == "uninitialized" {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE marker(value TEXT);")
                .unwrap();
            drop(conn);
            Some(fs::read(&path).unwrap())
        } else {
            None
        };
        let mut command = env.command(args);
        command.env("UTSURIGI_MEMORY_PATH", &path);
        let output = run(command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{layout}: {}",
            diagnostic(&output)
        );
        rejected(&output);
        match before {
            Some(before) => assert_eq!(fs::read(&path).unwrap(), before, "{layout}"),
            None => assert!(!path.exists(), "read created {}", path.display()),
        }
        if layout == "missing-parent" {
            assert!(!env.root.join("missing").exists());
        }
    }
}

#[test]
fn read_only_info_never_creates_a_database_or_schema() {
    read_only_rejects_missing_and_uninitialized_database(&["info"]);
}

#[test]
fn read_only_list_never_creates_a_database_or_schema() {
    read_only_rejects_missing_and_uninitialized_database(&["--json", "list"]);
}

#[test]
fn read_only_search_never_creates_a_database_or_schema() {
    read_only_rejects_missing_and_uninitialized_database(&["--hash", "--json", "search", "record"]);
}

#[test]
fn read_only_missing_search_fails_before_loading_a_model_or_spawning_a_daemon() {
    let env = Sandbox::new();
    let mut command = env.command(&["search", "record"]);
    command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
    let output = run(command);
    rejected(&output);
    let detail = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(detail.contains("database"), "{}", diagnostic(&output));
    assert!(!detail.contains("model"), "{}", diagnostic(&output));
    assert!(!env.db().exists());
    assert!(!env.root.join("cache/utsurigi-memory.lock").exists());
    assert!(!env.root.join("cache/utsurigi-memory.log").exists());
}

#[test]
fn read_only_snapshot_missing_source_creates_neither_source_nor_destination() {
    let env = Sandbox::new();
    let source = env.root.join("missing/nested/memory.db");
    let destination = env.root.join("exports/nested/snapshot.db");
    let output = env.run(&[
        "--db",
        source.to_str().unwrap(),
        "snapshot",
        destination.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    rejected(&output);
    assert!(String::from_utf8_lossy(&output.stderr)
        .starts_with("error: snapshot source: open database read-only:"));
    assert!(!env.root.join("missing").exists());
    assert!(!destination.exists());
    assert!(!env.root.join("exports").exists());
}

#[test]
fn read_only_search_does_not_pin_an_existing_unpinned_database() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection().execute("DELETE FROM meta", []).unwrap();
    let before = fs::read(env.db()).unwrap();
    let output = env.run(&["--hash", "search", "record"]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("embedder"));
    assert_eq!(fs::read(env.db()).unwrap(), before);
}

#[test]
fn read_only_info_before_snapshot_does_not_block_restoration() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let destination = env.root.join("restored/nested/restored.db");
    let output = env.run(&["--db", destination.to_str().unwrap(), "info"]);
    rejected(&output);
    assert!(!destination.exists());
    assert!(!env.root.join("restored").exists());
    success(&env.run(&["snapshot", destination.to_str().unwrap()]));
    let rows = env.json(&["--db", destination.to_str().unwrap(), "--json", "list"]);
    assert_eq!(rows[0]["id"], 41);
    assert_eq!(rows[0]["text"], "古い記録 🦀");
    assert_eq!(rows[0]["created_at"], "2023-11-14T22:13:20Z");
}

fn socket_path_rejected(command: Command) {
    let output = run(command);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    let detail = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(detail.contains("socket"), "{}", diagnostic(&output));
    assert!(!detail.contains("model"), "{}", diagnostic(&output));
    assert!(!detail.contains("panicked"), "{}", diagnostic(&output));
}

#[test]
fn socket_length_default_is_checked_before_model_loading() {
    let env = Sandbox::new();
    let cache = env.root.join("x".repeat(100));
    let mut command = env.command(&["serve"]);
    command.env_remove("UTSURIGI_MEMORY_SOCK");
    command.env("UTSURIGI_MEMORY_CACHE", &cache);
    socket_path_rejected(command);
    assert!(!cache.join("utsurigi-memory.sock").exists());
    assert!(!env.db().exists());
}

#[test]
fn socket_length_explicit_status_and_stop_are_rejected() {
    for args in [&["--json", "status"][..], &["stop"][..]] {
        let env = Sandbox::new();
        let mut command = env.command(args);
        command.env("UTSURIGI_MEMORY_SOCK", "s".repeat(108));
        socket_path_rejected(command);
        assert!(!env.db().exists());
    }
}

#[test]
fn socket_length_counts_utf8_bytes_before_model_loading() {
    let env = Sandbox::new();
    let socket = env.root.join("記".repeat(30));
    assert!(socket.to_str().unwrap().chars().count() < 108);
    assert!(socket.as_os_str().as_bytes().len() >= 108);
    let mut command = env.command(&["serve"]);
    command.env("UTSURIGI_MEMORY_SOCK", &socket);
    socket_path_rejected(command);
    assert!(!socket.exists());
}

#[test]
fn socket_length_is_checked_before_spawning_a_daemon() {
    let env = Sandbox::new();
    let mut command = env.command(&["add", "record"]);
    command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
    command.env("UTSURIGI_MEMORY_SOCK", "s".repeat(108));
    socket_path_rejected(command);
    assert!(!env.db().exists());
    assert!(!env.root.join("cache/utsurigi-memory.lock").exists());
    assert!(!env.root.join("cache/utsurigi-memory.log").exists());
}

#[test]
fn socket_unused_long_paths_do_not_block_database_commands() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let destination = env.root.join("snapshot.db");
    for args in [
        vec!["info"],
        vec!["--json", "list"],
        vec!["--hash", "search", "record"],
        vec!["--hash", "add", "record"],
        vec!["snapshot", destination.to_str().unwrap()],
    ] {
        let mut command = env.command(&args);
        command.env("UTSURIGI_MEMORY_SOCK", "s".repeat(108));
        success(&run(command));
    }
    assert!(destination.is_file());
    assert!(!env.root.join("cache/utsurigi-memory.lock").exists());
}

#[test]
fn created_at_rfc3339_instants_survive_add_list_search_and_snapshot() {
    let env = Sandbox::new();
    let cases = [
        ("2023-11-14T22:13:20Z", 1700000000, "2023-11-14T22:13:20Z"),
        (
            "2023-11-15T07:13:20+09:00",
            1700000000,
            "2023-11-14T22:13:20Z",
        ),
        (
            "2023-11-14T22:13:20.987Z",
            1700000000,
            "2023-11-14T22:13:20Z",
        ),
        ("1969-12-31T23:59:59Z", -1, "1969-12-31T23:59:59Z"),
    ];
    let mut expected = Vec::new();
    for (input, timestamp, json_time) in cases {
        let added = env.json(&[
            "--hash",
            "--json",
            "add",
            "imported 記録",
            "--tag",
            "migration",
            "--created-at",
            input,
        ]);
        let id = added["id"].as_i64().unwrap();
        let stored: (String, String, i64) = env
            .connection()
            .query_row(
                "SELECT text, tag, created_at FROM memories WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            stored,
            ("imported 記録".into(), "migration".into(), timestamp)
        );
        let rows = env.json(&["--json", "list"]);
        assert_eq!(
            rows.as_array().unwrap().last().unwrap()["created_at"],
            json_time
        );
        let hits = env.json(&["--hash", "--json", "search", "imported 記録", "-k", "10"]);
        let hit = hits
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(hit["created_at"], json_time);
        expected.push((id, timestamp));
    }
    let destination = env.root.join("dated-snapshot.db");
    success(&env.run(&["snapshot", destination.to_str().unwrap()]));
    let snapshot =
        Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let records = snapshot
        .prepare("SELECT id, created_at FROM memories ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(records, expected);
}

#[test]
fn created_at_json_round_trips_into_add_with_utc_seconds() {
    let source = Sandbox::new();
    source.json(&[
        "--hash",
        "--json",
        "add",
        "round-trip 記録",
        "--tag",
        "imported",
        "--created-at",
        "2026-10-21T16:00:37.987+09:00",
    ]);
    let rows = source.json(&["--json", "list"]);
    let row = &rows[0];
    let created_at = row["created_at"].as_str().unwrap();
    assert_eq!(created_at, "2026-10-21T07:00:37Z");

    let destination = Sandbox::new();
    let added = destination.json(&[
        "--hash",
        "--json",
        "add",
        row["text"].as_str().unwrap(),
        "--tag",
        row["tag"].as_str().unwrap(),
        "--created-at",
        created_at,
    ]);
    let timestamp: i64 = destination
        .connection()
        .query_row(
            "SELECT created_at FROM memories WHERE id=?1",
            [added["id"].as_i64().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(timestamp, 1792566037);
    let restored = destination.json(&["--json", "list"]);
    assert_eq!(restored[0]["text"], "round-trip 記録");
    assert_eq!(restored[0]["tag"], "imported");
    assert_eq!(restored[0]["created_at"], "2026-10-21T07:00:37Z");

    let human = source.run(&["list"]);
    success(&human);
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        "#1  2026-10-21 07:00 UTC [imported]  round-trip 記録\n"
    );
    let hits = source.json(&["--hash", "--json", "search", "round-trip 記録"]);
    assert_eq!(hits[0]["created_at"], "2026-10-21T07:00:37Z");
}

#[test]
fn created_at_omitted_still_uses_current_utc_seconds() {
    let env = Sandbox::new();
    let before = chrono::Utc::now().timestamp();
    let added = env.json(&["--hash", "--json", "add", "current record"]);
    let after = chrono::Utc::now().timestamp();
    let timestamp: i64 = env
        .connection()
        .query_row(
            "SELECT created_at FROM memories WHERE id=?1",
            [added["id"].as_i64().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert!((before..=after).contains(&timestamp));
}

#[test]
fn created_at_invalid_values_leave_missing_and_existing_databases_untouched() {
    for input in [
        "",
        "yesterday",
        "2023-11-14",
        "2023-02-30T22:13:20Z",
        "2023-11-14T22:13:20",
    ] {
        for existing in [false, true] {
            let env = Sandbox::new();
            let path = if existing {
                env.legacy(HASH);
                env.db()
            } else {
                env.root.join("missing/nested/memory.db")
            };
            let before = fs::read(&path).ok();
            let mut command = env.command(&["--hash", "add", "record", "--created-at", input]);
            command.env("UTSURIGI_MEMORY_PATH", &path);
            let output = run(command);
            assert_eq!(
                output.status.code(),
                Some(1),
                "{input:?}: {}",
                diagnostic(&output)
            );
            let detail = String::from_utf8_lossy(&output.stderr);
            assert!(
                detail.contains("--created-at") && detail.contains("RFC3339"),
                "{detail}"
            );
            assert_eq!(fs::read(&path).ok(), before);
            assert!(!env.root.join("missing").exists());
        }
    }
}

#[test]
fn created_at_missing_or_duplicate_values_fail_before_creating_a_database() {
    for options in [
        vec!["--created-at"],
        vec!["--created-at", "--tag", "fixture"],
        vec![
            "--created-at",
            "2023-11-14T22:13:20Z",
            "--created-at",
            "2024-01-01T00:00:00Z",
        ],
    ] {
        let env = Sandbox::new();
        let mut command = env.command(&["--hash", "add", "record"]);
        command.args(options);
        let output = run(command);
        assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
        assert!(String::from_utf8_lossy(&output.stderr).contains("--created-at"));
        assert!(!env.db().exists());
    }
}

#[test]
fn created_at_is_rejected_for_other_commands_without_side_effects() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let before = fs::read(env.db()).unwrap();
    for args in [
        vec!["info"],
        vec!["list"],
        vec!["--hash", "search", "record"],
        vec!["serve"],
        vec!["status"],
        vec!["stop"],
        vec!["snapshot", "unused.db"],
    ] {
        let mut command = env.command(&args);
        command.args(["--created-at", "2023-11-14T22:13:20Z"]);
        let output = run(command);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            diagnostic(&output)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("--created-at"));
        assert_eq!(fs::read(env.db()).unwrap(), before);
    }
    assert!(!env.root.join("unused.db").exists());
    assert!(!env.root.join("sock").exists());
}

#[test]
fn write_error_add_returns_a_cli_error_instead_of_panicking() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute_batch(
            "CREATE TRIGGER block_insert BEFORE INSERT ON memories
         BEGIN SELECT RAISE(ABORT, 'fixture insert blocked'); END;",
        )
        .unwrap();
    let before = fs::read(env.db()).unwrap();
    let output = env.run(&["--hash", "add", "blocked record"]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    let detail = String::from_utf8_lossy(&output.stderr);
    assert!(
        detail.starts_with("error:") && detail.contains("fixture insert blocked"),
        "{detail}"
    );
    assert!(!detail.contains("panicked"), "{detail}");
    assert_eq!(fs::read(env.db()).unwrap(), before);
}

#[test]
fn delete_missing_file_does_not_create_a_database() {
    let env = Sandbox::new();
    let output = env.run(&["--json", "delete", "41"]);
    assert!(
        !env.db().exists(),
        "delete created a database: {}",
        diagnostic(&output)
    );
    assert_eq!(fs::read_dir(env.root.join("db")).unwrap().count(), 0);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    assert!(output.stdout.is_empty(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("error: open database:"),
        "{}",
        diagnostic(&output)
    );
}

#[test]
fn delete_missing_parent_does_not_create_directories() {
    let env = Sandbox::new();
    let path = env.root.join("missing/nested/memory.db");
    let output = env.run(&["--db", path.to_str().unwrap(), "delete", "41"]);
    assert!(
        !env.root.join("missing").exists(),
        "delete created parent directories: {}",
        diagnostic(&output)
    );
    assert!(!path.exists());
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    assert!(output.stdout.is_empty(), "{}", diagnostic(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("error: open database:"),
        "{}",
        diagnostic(&output)
    );
}

#[test]
fn write_error_delete_returns_a_cli_error_instead_of_panicking() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute_batch(
            "CREATE TRIGGER block_delete BEFORE DELETE ON memories
         BEGIN SELECT RAISE(ABORT, 'fixture delete blocked'); END;",
        )
        .unwrap();
    let before = fs::read(env.db()).unwrap();
    let output = env.run(&["delete", "41"]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    let detail = String::from_utf8_lossy(&output.stderr);
    assert!(
        detail.starts_with("error:") && detail.contains("fixture delete blocked"),
        "{detail}"
    );
    assert!(!detail.contains("panicked"), "{detail}");
    assert_eq!(fs::read(env.db()).unwrap(), before);
}

fn list_to_closed_pipe(args: &[&str]) {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute(
            "WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<128)
             INSERT INTO memories(text, tag, created_at, vec)
             SELECT ?1, NULL, 1700000000, x'' FROM ids",
            ["large listing ".repeat(256)],
        )
        .unwrap();
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = env.command(args).stdout(writer).output().unwrap();
    success(&output);
    assert!(output.stderr.is_empty(), "{}", diagnostic(&output));
}

#[test]
fn list_human_exits_quietly_when_stdout_pipe_closes() {
    list_to_closed_pipe(&["list"]);
}

#[test]
fn list_json_exits_quietly_when_stdout_pipe_closes() {
    list_to_closed_pipe(&["--json", "list"]);
}

#[test]
#[cfg(target_os = "linux")]
fn list_reports_stdout_errors_other_than_broken_pipe() {
    let env = Sandbox::new();
    env.legacy(HASH);
    for args in [vec!["list"], vec!["--json", "list"]] {
        let output = env
            .command(&args)
            .stdout(
                fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/full")
                    .unwrap(),
            )
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("error: stdout:"),
            "{}",
            diagnostic(&output)
        );
    }
}

#[test]
fn preservation_unicode_hash_add_search_delete_persist() {
    let env = Sandbox::new();
    let added = env.json(&[
        "--hash",
        "--json",
        "add",
        "京都の図書館 📚",
        "--tag",
        "旅行",
    ]);
    assert_eq!(added["embedder"], HASH);
    let id = added["id"].as_i64().unwrap();
    env.json(&["--hash", "--json", "add", "Rust compiler notes"]);
    let listed = env.json(&["--json", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["text"], "京都の図書館 📚");
    assert_eq!(listed[0]["tag"], "旅行");
    let hits = env.json(&["--hash", "--json", "search", "京都の図書館 📚", "-k", "1"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"], id);
    assert!(hits[0]["score"].as_f64().unwrap() > 0.99);
    let deleted = env.json(&["--json", "delete", &format!("#{id}")]);
    assert_eq!(deleted["deleted"], true);
    let remaining = env.json(&["--json", "list"]);
    assert_eq!(remaining.as_array().unwrap().len(), 1);
    assert_eq!(remaining[0]["text"], "Rust compiler notes");
    assert!(!env.run(&["delete", &id.to_string()]).status.success());
}

#[test]
fn preservation_text_commands_and_info_need_no_model() {
    let env = Sandbox::new();
    let added = env.run(&["--hash", "add", "雪の日 ☃", "--tag", "日記"]);
    success(&added);
    let id: i64 = String::from_utf8(added.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let listed = env.run(&["list"]);
    success(&listed);
    let text = String::from_utf8(listed.stdout).unwrap();
    assert!(
        text.contains(&format!("#{id}")) && text.contains("[日記]") && text.contains("雪の日 ☃")
    );
    let searched = env.run(&["--hash", "search", "雪の日 ☃", "-k", "1"]);
    success(&searched);
    assert!(String::from_utf8_lossy(&searched.stdout).contains("雪の日 ☃"));
    let daemon_command = |args: &[&str]| {
        let mut command = env.command(args);
        command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
        command
    };
    let output = run(daemon_command(&["info"]));
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("db: {}", env.db().display())));
    assert!(text.contains(&format!("embedder: {HASH}")) && text.contains("count: 1"));
    assert!(output.stderr.is_empty());
    assert!(!env.root.join("sock").exists());
    assert!(!env.root.join("cache/model.onnx").exists());
    for name in ["utsurigi-memory.lock", "utsurigi-memory.log"] {
        assert!(!env.root.join("cache").join(name).exists());
    }
    let deleted = env.run(&["delete", &id.to_string()]);
    success(&deleted);
    assert!(String::from_utf8_lossy(&deleted.stdout).contains(&format!("deleted #{id}")));
    rejected(&run(daemon_command(&["search", "daemon path control"])));
    env.daemon_attempted();
}

#[test]
fn preservation_db_flag_overrides_memory_path() {
    let env = Sandbox::new();
    let other = env.root.join("other.db");
    success(&env.run(&["--db", other.to_str().unwrap(), "--hash", "add", "override"]));
    assert!(other.is_file());
    assert!(!env.db().exists());
    assert!(!env.root.join("package/memory.db").exists());
}

#[test]
fn preservation_memory_home_supplies_default_database() {
    let env = Sandbox::new();
    let mut command = env.command(&["--hash", "add", "package override"]);
    command.env_remove("UTSURIGI_MEMORY_PATH");
    success(&run(command));
    assert!(env.root.join("package/memory.db").is_file());
    assert!(!env.db().exists());
    assert!(!env.root.join("data/utsurigi-memory/memory.db").exists());
    assert!(!env
        .root
        .join("home/.local/share/utsurigi-memory/memory.db")
        .exists());
}

#[test]
fn preservation_status_and_stop_use_isolated_socket_without_database() {
    let env = Sandbox::new();
    assert_eq!(env.json(&["--json", "status"])["running"], false);
    let stopped = env.run(&["stop"]);
    success(&stopped);
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("not running"));
    assert!(!env.db().exists());
}

#[test]
fn preservation_status_and_stop_follow_memory_sock_protocol() {
    let env = Sandbox::new();
    let listener = UnixListener::bind(env.root.join("sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut operations = Vec::new();
        for response in [
            r#"{"pid":73,"embedder":"char-ngram-hash-1024","idle_timeout_s":2}"#,
            r#"{"ok":true}"#,
        ] {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "no request on UTSURIGI_MEMORY_SOCK"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("accept protocol peer: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            operations.push(request["op"].as_str().unwrap().to_owned());
            writeln!(stream, "{response}").unwrap();
        }
        operations
    });
    assert_eq!(env.json(&["--json", "status"])["pid"], 73);
    let stopped = env.run(&["stop"]);
    success(&stopped);
    assert_eq!(String::from_utf8_lossy(&stopped.stdout).trim(), "stopped");
    assert_eq!(peer.join().unwrap(), ["status", "stop"]);
    assert!(!env.db().exists());
}

#[test]
fn preservation_legacy_hash_database_remains_readable_with_explicit_hash() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let rows = env.json(&["--json", "list"]);
    assert_eq!(rows[0]["id"], 41);
    assert_eq!(rows[0]["text"], "古い記録 🦀");
    assert_eq!(rows[0]["tag"], "legacy");
    let hits = env.json(&["--hash", "--json", "search", "記録"]);
    assert_eq!(hits.as_array().unwrap().len(), 1);
    assert_eq!(hits[0]["id"], 41);
    let added = env.json(&["--hash", "--json", "add", "新しい記録"]);
    assert_eq!(added["id"], 42);
    assert_eq!(env.json(&["--json", "list"]).as_array().unwrap().len(), 2);
}

#[test]
fn preservation_hash_refuses_a_different_vector_space() {
    let env = Sandbox::new();
    env.legacy("multilingual-e5-small");
    let output = env.run(&["--hash", "add", "must not mix"]);
    rejected(&output);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(HASH) && error.contains("multilingual-e5-small"));
    assert_eq!(env.json(&["--json", "list"]).as_array().unwrap().len(), 1);
}

#[test]
fn default_home_uses_xdg_data_home() {
    let env = Sandbox::new();
    fs::create_dir_all(env.root.join("data/utsurigi-memory")).unwrap();
    let mut command = env.command(&["--hash", "add", "XDG default"]);
    command
        .env_remove("UTSURIGI_MEMORY_HOME")
        .env_remove("UTSURIGI_MEMORY_PATH");
    success(&run(command));
    assert!(env.root.join("data/utsurigi-memory/memory.db").is_file());
    assert!(!env
        .root
        .join("home/.local/share/utsurigi-memory/memory.db")
        .exists());
}

#[test]
fn default_home_falls_back_to_home_local_share() {
    let env = Sandbox::new();
    fs::create_dir_all(env.root.join("home/.local/share/utsurigi-memory")).unwrap();
    let mut command = env.command(&["--hash", "add", "HOME default"]);
    command
        .env_remove("UTSURIGI_MEMORY_HOME")
        .env_remove("UTSURIGI_MEMORY_PATH")
        .env_remove("XDG_DATA_HOME");
    success(&run(command));
    assert!(env
        .root
        .join("home/.local/share/utsurigi-memory/memory.db")
        .is_file());
    assert!(!env.root.join("data/utsurigi-memory/memory.db").exists());
}

#[test]
fn stored_timestamps_are_displayed_in_utc() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let rows = env.json(&["--json", "list"]);
    let timestamp = rows[0]["created_at"].as_str().unwrap();
    assert!(
        timestamp.contains("2023-11-14") && timestamp.contains("22:13"),
        "{timestamp}"
    );
    for args in [vec!["list"], vec!["--hash", "search", "記録"]] {
        let output = env.run(&args);
        success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("2023-11-14") && text.contains("22:13"),
            "{text}"
        );
    }
}

#[test]
fn missing_model_fails_without_creating_a_hash_database() {
    let env = Sandbox::new();
    let output = env.run(&["add", "no implicit hash"]);
    env.model_failure(&output, None);
    assert!(String::from_utf8_lossy(&output.stderr)
        .to_lowercase()
        .contains("model"));
}

#[test]
fn missing_model_daemon_path_fails_without_creating_a_hash_database() {
    let env = Sandbox::new();
    let mut command = env.command(&["add", "no implicit daemon hash"]);
    command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
    let output = run(command);
    env.model_failure(&output, None);
    env.daemon_attempted();
}

#[test]
fn missing_model_preserves_an_existing_unpinned_database() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute_batch("DELETE FROM memories; DELETE FROM meta;")
        .unwrap();
    let before = fs::read(env.db()).unwrap();
    let output = env.run(&["add", "leave this database alone"]);
    env.model_failure(&output, Some(&before));
}

#[test]
fn legacy_hash_database_still_requires_explicit_hash_when_model_is_missing() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let before = fs::read(env.db()).unwrap();
    env.model_failure(&env.run(&["--json", "search", "記録"]), Some(&before));
    assert_eq!(env.json(&["--json", "list"])[0]["id"], 41);
}

#[test]
fn invalid_model_fails_instead_of_falling_back_to_hash() {
    let env = Sandbox::new();
    env.model("invalid");
    let output = env.run(&["add", "invalid ONNX"]);
    env.model_failure(&output, None);
}

#[test]
fn corrupt_model_preserves_an_existing_hash_database() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.model("invalid");
    let before = fs::read(env.db()).unwrap();
    let output = env.run(&["add", "must not change legacy records"]);
    env.model_failure(&output, Some(&before));
}

#[test]
fn corrupt_model_daemon_path_leaves_no_database_or_socket() {
    let env = Sandbox::new();
    env.model("invalid");
    let mut command = env.command(&["add", "no corrupt daemon model"]);
    command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
    env.model_failure(&run(command), None);
    env.daemon_attempted();
}

fn checksum_rejection(layout: &str) {
    let env = Sandbox::new();
    env.model(layout);
    let output = env.run(&["add", "checksum guard"]);
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .to_lowercase()
            .contains("checksum"),
        "{}",
        diagnostic(&output)
    );
    env.model_failure(&output, None);
}

#[test]
fn direct_model_checksum_mismatch_is_visible() {
    checksum_rejection("direct");
}

#[test]
fn chunked_model_checksum_mismatch_is_visible() {
    checksum_rejection("chunks");
}

#[test]
fn same_size_cached_model_checksum_mismatch_is_visible() {
    checksum_rejection("cached");
}

fn corrupt_vector(blob: &[u8]) {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute("UPDATE memories SET vec=?1", [blob])
        .unwrap();
    rejected(&env.run(&["--hash", "--json", "search", "記録"]));
}

#[test]
fn truncated_vector_blob_fails_visibly() {
    corrupt_vector(&[1, 2, 3]);
}

#[test]
fn wrong_vector_dimension_fails_visibly() {
    corrupt_vector(&1f32.to_le_bytes());
}

#[test]
fn wrong_e5_vector_dimension_fails_visibly() {
    let env = Sandbox::new();
    env.legacy("multilingual-e5-small");
    let mut bytes = vec![0; 1024 * 4];
    bytes[..4].copy_from_slice(&1f32.to_le_bytes());
    env.connection()
        .execute("UPDATE memories SET vec=?1", [bytes])
        .unwrap();
    let listener = UnixListener::bind(env.root.join("sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "no embed request on UTSURIGI_MEMORY_SOCK"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("accept embedding peer: {e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            if line.is_empty() {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "embed");
            assert_eq!(request["query"], true);
            let mut vector = vec![0.0; 384];
            vector[0] = 1.0;
            writeln!(
                stream,
                "{}",
                serde_json::json!({"embedder": "multilingual-e5-small", "vec": vector})
            )
            .unwrap();
            break;
        }
    });
    let mut command = env.command(&["--json", "search", "記録"]);
    command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
    let output = run(command);
    peer.join().unwrap();
    rejected(&output);
}

fn corrupt_float(value: f32) {
    let mut bytes = vec![0; 1024 * 4];
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    corrupt_vector(&bytes);
}

#[test]
fn non_finite_vector_fails_visibly() {
    corrupt_float(f32::NAN);
}

#[test]
fn positive_infinite_vector_fails_visibly() {
    corrupt_float(f32::INFINITY);
}

#[test]
fn negative_infinite_vector_fails_visibly() {
    corrupt_float(f32::NEG_INFINITY);
}

#[test]
fn sqlite_row_conversion_errors_are_not_silently_discarded() {
    let env = Sandbox::new();
    env.legacy(HASH);
    env.connection()
        .execute("UPDATE memories SET text=x'ff'", [])
        .unwrap();
    rejected(&env.run(&["--json", "list"]));
}

#[test]
fn snapshot_preserves_committed_wal_records_and_vectors_without_inference() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let source = env.connection();
    source
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    source
        .execute(
            "INSERT INTO memories(text, tag, created_at, vec)
         SELECT '追記された記録 🌸', NULL, 1700000001, vec FROM memories WHERE id=41",
            [],
        )
        .unwrap();
    assert!(env.db().with_extension("db-wal").exists());
    let destination = env.root.join("snapshots/nested/snapshot.db");
    assert!(!env.root.join("snapshots").exists());
    let daemon_command = |args: &[&str]| {
        let mut command = env.command(args);
        command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
        command
    };
    success(&run(daemon_command(&[
        "snapshot",
        "snapshots/nested/snapshot.db",
    ])));
    let standalone = env.root.join("standalone.db");
    fs::copy(&destination, &standalone).unwrap();
    let snapshot =
        Connection::open_with_flags(&standalone, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let integrity: String = snapshot
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let records = |conn: &Connection| {
        conn.prepare("SELECT id, text, tag, created_at, vec FROM memories ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Vec<u8>>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(records(&snapshot), records(&source));
    let embedder: String = snapshot
        .query_row("SELECT value FROM meta WHERE key='embedder'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(embedder, HASH);
    source
        .execute("DELETE FROM memories WHERE id=41", [])
        .unwrap();
    assert_eq!(records(&snapshot).len(), 2);
    assert!(!env.root.join("sock").exists());
    assert!(!env.root.join("cache/model.onnx").exists());
    rejected(&run(daemon_command(&["search", "daemon path control"])));
    env.daemon_attempted();
}

#[test]
fn snapshot_is_private_with_a_permissive_child_umask() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let destination = env.root.join("private/nested/snapshot.db");
    let mut command = Command::new("/bin/bash");
    command
        .args(["-c", "umask 000; exec \"$@\"", "snapshot-umask"])
        .arg(env!("CARGO_BIN_EXE_utsurigi-memory"))
        .arg("--db")
        .arg(env.db())
        .arg("snapshot")
        .arg(&destination)
        .env_clear()
        .env("HOME", env.root.join("home"))
        .current_dir(&env.root);
    let output = run(command);
    success(&output);
    assert!(output.stderr.is_empty(), "{}", diagnostic(&output));
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let rows = env.json(&["--db", destination.to_str().unwrap(), "--json", "list"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], 41);
    assert_eq!(rows[0]["text"], "古い記録 🦀");
    assert_eq!(rows[0]["tag"], "legacy");
    assert_eq!(rows[0]["created_at"], "2023-11-14T22:13:20Z");
}

#[test]
fn snapshot_refuses_existing_and_dangling_symlinks() {
    for existing_target in [false, true] {
        let env = Sandbox::new();
        env.legacy(HASH);
        let target = env.root.join("target.db");
        if existing_target {
            fs::write(&target, b"keep this target").unwrap();
        }
        let destination = env.root.join("snapshot.db");
        symlink(&target, &destination).unwrap();
        let output = env.run(&["snapshot", destination.to_str().unwrap()]);
        rejected(&output);
        assert_eq!(fs::read_link(&destination).unwrap(), target);
        if existing_target {
            assert_eq!(fs::read(&target).unwrap(), b"keep this target");
        } else {
            assert!(!target.exists());
        }
    }
}

#[test]
fn snapshot_removes_incomplete_destination_after_export_error() {
    let env = Sandbox::new();
    fs::write(env.db(), b"invalid sqlite database").unwrap();
    let destination = env.root.join("failed/snapshot.db");
    let output = env.run(&["snapshot", destination.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: snapshot:"));
    assert!(!destination.exists());
    assert_eq!(fs::read(env.db()).unwrap(), b"invalid sqlite database");
}

#[test]
fn snapshot_refuses_existing_files_and_the_source_database() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let fresh = env.root.join("fresh.db");
    success(&env.run(&["snapshot", fresh.to_str().unwrap()]));
    let existing = env.root.join("existing.db");
    fs::write(&existing, b"keep this destination").unwrap();
    rejected(&env.run(&["snapshot", existing.to_str().unwrap()]));
    assert_eq!(fs::read(&existing).unwrap(), b"keep this destination");
    fs::write(&existing, b"").unwrap();
    rejected(&env.run(&["snapshot", existing.to_str().unwrap()]));
    assert!(fs::read(&existing).unwrap().is_empty());
    let original = fs::read(env.db()).unwrap();
    rejected(&env.run(&["snapshot", env.db().to_str().unwrap()]));
    assert_eq!(fs::read(env.db()).unwrap(), original);
}

#[test]
fn xdg_empty_data_home_falls_back_to_home_local_share() {
    let env = Sandbox::new();
    let mut command = env.command(&["--hash", "add", "empty XDG data home"]);
    command
        .env_remove("UTSURIGI_MEMORY_HOME")
        .env_remove("UTSURIGI_MEMORY_PATH")
        .env("XDG_DATA_HOME", "");
    let output = run(command);
    success(&output);
    assert!(
        env.root.join("home/.local/share/utsurigi-memory/memory.db").is_file(),
        "empty XDG_DATA_HOME must use HOME/.local/share/utsurigi-memory; relative database exists={}: {}",
        env.root.join("utsurigi-memory/memory.db").exists(),
        diagnostic(&output)
    );
    assert!(!env.root.join("utsurigi-memory").exists());
    assert!(!env.root.join("package/memory.db").exists());
    assert!(!env.db().exists());
}

#[test]
fn xdg_empty_cache_home_falls_back_to_home_cache() {
    let env = Sandbox::new();
    let mut command = env.command(&["--json", "status"]);
    command
        .env_remove("UTSURIGI_MEMORY_CACHE")
        .env_remove("UTSURIGI_MEMORY_SOCK")
        .env("XDG_CACHE_HOME", "");
    let output = status_at(command, &env.root.join("home/.cache/utsurigi-memory"));
    success(&output);
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["running"], false);
    assert!(
        env.root.join("home/.cache/utsurigi-memory").is_dir(),
        "empty XDG_CACHE_HOME must use HOME/.cache/utsurigi-memory; relative cache exists={}: {}",
        env.root.join("utsurigi-memory").exists(),
        diagnostic(&output)
    );
    assert!(!env.root.join("utsurigi-memory").exists());
    assert!(!env.db().exists());
    assert!(!env.root.join("sock").exists());
}

#[test]
fn xdg_cache_path_controls() {
    for (xdg, explicit, expected) in [
        (None, None, "home/.cache/utsurigi-memory"),
        (Some("xdg-cache"), None, "xdg-cache/utsurigi-memory"),
        (Some("xdg-cache"), Some("explicit-cache"), "explicit-cache"),
    ] {
        let env = Sandbox::new();
        let mut command = env.command(&["--json", "status"]);
        command
            .env_remove("UTSURIGI_MEMORY_CACHE")
            .env_remove("UTSURIGI_MEMORY_SOCK")
            .env_remove("XDG_CACHE_HOME");
        if let Some(xdg) = xdg {
            command.env("XDG_CACHE_HOME", env.root.join(xdg));
        }
        if let Some(explicit) = explicit {
            command.env("UTSURIGI_MEMORY_CACHE", env.root.join(explicit));
        }
        let output = status_at(command, &env.root.join(expected));
        success(&output);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["running"], false);
        for candidate in [
            "home/.cache/utsurigi-memory",
            "xdg-cache/utsurigi-memory",
            "explicit-cache",
            "utsurigi-memory",
        ] {
            assert_eq!(
                env.root.join(candidate).is_dir(),
                candidate == expected,
                "XDG_CACHE_HOME={xdg:?} UTSURIGI_MEMORY_CACHE={explicit:?}: {candidate}: {}",
                diagnostic(&output)
            );
        }
        assert!(!env.db().exists());
        assert!(!env.root.join("sock").exists());
    }
}

#[test]
fn empty_memory_home_uses_xdg_or_home_in_hash_mode() {
    let mut missing = Vec::new();
    for (xdg, expected) in [
        ("data", "data/utsurigi-memory/memory.db"),
        ("", "home/.local/share/utsurigi-memory/memory.db"),
    ] {
        let env = Sandbox::new();
        let mut command = env.command(&["--hash", "--json", "add", "synthetic fallback memory"]);
        command
            .env("UTSURIGI_MEMORY_HOME", "")
            .env_remove("UTSURIGI_MEMORY_PATH");
        command.env(
            "XDG_DATA_HOME",
            if xdg.is_empty() {
                PathBuf::new()
            } else {
                env.root.join(xdg)
            },
        );
        let output = run(command);
        success(&output);
        let added: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(added["embedder"], HASH);
        if !env.root.join(expected).is_file() || env.root.join("memory.db").exists() {
            missing.push(format!(
                "empty UTSURIGI_MEMORY_HOME with XDG_DATA_HOME={xdg:?} must write {expected}; {}",
                diagnostic(&output)
            ));
        }
        assert!(!env.db().exists());
        assert!(!env.root.join("package/memory.db").exists());
        assert!(!env.root.join("cache/model.onnx").exists());
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn empty_memory_cache_uses_xdg_or_home_without_loading_a_model() {
    let mut missing = Vec::new();
    for (xdg, expected) in [
        ("xdg-cache", "xdg-cache/utsurigi-memory"),
        ("", "home/.cache/utsurigi-memory"),
    ] {
        let env = Sandbox::new();
        let mut command = env.command(&["--json", "status"]);
        command
            .env("UTSURIGI_MEMORY_CACHE", "")
            .env_remove("UTSURIGI_MEMORY_SOCK");
        command.env(
            "XDG_CACHE_HOME",
            if xdg.is_empty() {
                PathBuf::new()
            } else {
                env.root.join(xdg)
            },
        );
        let output = status_at(command, &env.root.join(expected));
        if !output.status.success() || !env.root.join(expected).is_dir() {
            missing.push(format!(
                "empty UTSURIGI_MEMORY_CACHE with XDG_CACHE_HOME={xdg:?} must use {expected}; {}",
                diagnostic(&output)
            ));
        } else {
            let status: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(status["running"], false);
        }
        assert!(!env.db().exists());
        assert!(!env.root.join("sock").exists());
        assert!(!env.root.join("cache/model.onnx").exists());
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn control_nonempty_memory_home_and_cache_override_xdg() {
    let env = Sandbox::new();
    let mut command = env.command(&["--hash", "--json", "add", "synthetic explicit memory"]);
    command.env_remove("UTSURIGI_MEMORY_PATH");
    let output = run(command);
    success(&output);
    let added: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(added["embedder"], HASH);
    assert!(env.root.join("package/memory.db").is_file());
    assert!(!env.root.join("data/utsurigi-memory").exists());
    assert!(!env.root.join("home/.local/share/utsurigi-memory").exists());
    let mut command = env.command(&["--json", "status"]);
    command
        .env_remove("UTSURIGI_MEMORY_SOCK")
        .env("UTSURIGI_MEMORY_CACHE", env.root.join("explicit-cache"));
    success(&status_at(command, &env.root.join("explicit-cache")));
    assert!(env.root.join("explicit-cache").is_dir());
    assert!(!env.root.join("cache/utsurigi-memory").exists());
    assert!(!env.root.join("home/.cache/utsurigi-memory").exists());
    assert!(!env.root.join("memory.db").exists());
    assert!(!env.db().exists());
    assert!(!env.root.join("cache/model.onnx").exists());
}

fn memory_command(env: &Sandbox, args: &[&str]) -> Command {
    let mut command = env.command(args);
    for suffix in ["HOME", "PATH", "CACHE", "SOCK", "IDLE_SECS", "NO_DAEMON"] {
        command.env_remove(format!("UTSURIGI_MEMORY_{suffix}"));
    }
    command
}

fn socket_request(
    listener: UnixListener,
    command: Command,
    response: Value,
) -> (Output, Option<Value>) {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    listener.set_nonblocking(true).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let peer_finished = Arc::clone(&finished);
    let peer = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !peer_finished.load(Ordering::Relaxed) && Instant::now() < deadline {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("accept memory protocol peer: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            if line.is_empty() {
                continue;
            }
            let request = serde_json::from_str(&line).unwrap();
            writeln!(stream, "{response}").unwrap();
            return Some(request);
        }
        None
    });
    let output = run(command);
    finished.store(true, Ordering::Relaxed);
    (output, peer.join().unwrap())
}

#[test]
fn environment_path_preserves_records_across_invocations() {
    let env = Sandbox::new();
    let database = env.root.join("canonical.db");
    let mut add = memory_command(&env, &["--hash", "--json", "add", "persistent path 記録"]);
    add.env("UTSURIGI_MEMORY_PATH", &database);
    let added = run(add);
    success(&added);
    let added: Value = serde_json::from_slice(&added.stdout).unwrap();
    assert!(database.is_file());
    let mut list = memory_command(&env, &["--json", "list"]);
    list.env("UTSURIGI_MEMORY_PATH", &database);
    let listed = run(list);
    success(&listed);
    let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], added["id"]);
    assert_eq!(rows[0]["text"], "persistent path 記録");
    assert!(!env.root.join("data/utsurigi-memory/memory.db").exists());
    assert!(!env.db().exists());
    assert!(!env.root.join("memory.db").exists());
}

#[test]
fn empty_database_flag_is_rejected_before_opening_any_database() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let before = fs::read(env.db()).unwrap();
    for args in [
        vec!["--db", "", "--json", "list"],
        vec!["--db", "", "--hash", "add", "must not disappear"],
    ] {
        let mut command = memory_command(&env, &args);
        command.env("UTSURIGI_MEMORY_PATH", env.root.join("canonical.db"));
        let output = run(command);
        rejected(&output);
        assert!(String::from_utf8_lossy(&output.stderr).contains("--db"));
        assert_eq!(fs::read(env.db()).unwrap(), before);
        assert!(!env.root.join("canonical.db").exists());
        assert!(!env.root.join("data/utsurigi-memory").exists());
        assert!(!env.root.join("cache/utsurigi-memory").exists());
        assert!(!env.root.join("memory.db").exists());
    }
}

#[test]
fn default_database_paths_reopen_existing_records_without_environment_overrides() {
    for (xdg, expected) in [
        (true, "data/utsurigi-memory/memory.db"),
        (false, "home/.local/share/utsurigi-memory/memory.db"),
    ] {
        let env = Sandbox::new();
        let database = env.root.join(expected);
        let seed = memory_command(
            &env,
            &[
                "--db",
                database.to_str().unwrap(),
                "--hash",
                "--json",
                "add",
                "existing 記録",
            ],
        );
        let seeded = run(seed);
        success(&seeded);
        let seeded: Value = serde_json::from_slice(&seeded.stdout).unwrap();
        let mut command = memory_command(&env, &["--json", "list"]);
        if !xdg {
            command.env_remove("XDG_DATA_HOME");
        }
        let output = run(command);
        success(&output);
        let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["id"], seeded["id"]);
        assert_eq!(rows[0]["text"], "existing 記録");
        for candidate in [
            if xdg {
                "home/.local/share/utsurigi-memory"
            } else {
                "data/utsurigi-memory"
            },
            "package/memory.db",
            "db/memory.db",
        ] {
            assert!(!env.root.join(candidate).exists());
        }
    }
}

#[test]
fn environment_socket_and_empty_values_reach_the_selected_daemon() {
    for (value, expected) in [
        (Some("canonical.sock"), "canonical.sock"),
        (Some(""), "cache/utsurigi-memory/utsurigi-memory.sock"),
        (None, "cache/utsurigi-memory/utsurigi-memory.sock"),
    ] {
        let env = Sandbox::new();
        let socket = env.root.join(expected);
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        let mut command = memory_command(&env, &["--json", "status"]);
        command.env("UTSURIGI_MEMORY_SOCKET", env.root.join("ignored.sock"));
        if let Some(value) = value {
            command.env(
                "UTSURIGI_MEMORY_SOCK",
                if value.is_empty() {
                    PathBuf::new()
                } else {
                    env.root.join(value)
                },
            );
        }
        let (output, request) = socket_request(
            listener,
            command,
            serde_json::json!({"pid":73,"embedder":HASH,"idle_timeout_s":2}),
        );
        success(&output);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["pid"], 73, "SOCK={value:?}: {}", diagnostic(&output));
        assert_eq!(request.unwrap()["op"], "status");
        assert!(!env.db().exists());
        assert!(!env.root.join("data/utsurigi-memory/memory.db").exists());
    }
}

#[test]
fn no_daemon_uses_presence_including_empty_values() {
    for (value, disabled) in [(None, false), (Some(""), true), (Some("0"), true)] {
        let env = Sandbox::new();
        env.legacy("multilingual-e5-small");
        let before = fs::read(env.db()).unwrap();
        let listener = UnixListener::bind(env.root.join("sock")).unwrap();
        let mut command = env.command(&["--json", "search", "記録"]);
        command.env_remove("UTSURIGI_MEMORY_NO_DAEMON");
        if let Some(value) = value {
            command.env("UTSURIGI_MEMORY_NO_DAEMON", value);
        }
        let mut vector = vec![0.0; 384];
        vector[0] = 1.0;
        let (output, request) = socket_request(
            listener,
            command,
            serde_json::json!({"embedder":"multilingual-e5-small", "vec":vector}),
        );
        if disabled {
            rejected(&output);
            assert!(request.is_none(), "NO_DAEMON={value:?} contacted a daemon");
            assert!(String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("model"));
        } else {
            success(&output);
            let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(rows.as_array().unwrap().len(), 1);
            assert_eq!(rows[0]["id"], 41);
            let request = request.unwrap();
            assert_eq!(request["op"], "embed");
            assert_eq!(request["query"], true);
        }
        assert_eq!(fs::read(env.db()).unwrap(), before);
    }
}

#[test]
fn empty_database_environment_values_do_not_create_temporary_databases() {
    for value in [Some(""), None] {
        let env = Sandbox::new();
        let mut add = memory_command(&env, &["--hash", "--json", "add", "empty path 記録"]);
        if let Some(value) = value {
            add.env("UTSURIGI_MEMORY_PATH", value);
        }
        success(&run(add));
        assert!(
            env.root.join("data/utsurigi-memory/memory.db").is_file(),
            "PATH={value:?} must select a persistent default database"
        );
        let mut list = memory_command(&env, &["--json", "list"]);
        if let Some(value) = value {
            list.env("UTSURIGI_MEMORY_PATH", value);
        }
        let listed = run(list);
        success(&listed);
        let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["text"], "empty path 記録");
        assert!(!env.db().exists());
        assert!(!env.root.join("memory.db").exists());
    }
}

#[test]
fn trailing_database_flag_without_value_cannot_add_or_delete_existing_records() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let before = fs::read(env.db()).unwrap();
    let added = env.run(&["--hash", "add", "must not be stored", "--db"]);
    let after_add = fs::read(env.db()).unwrap();
    let deleted = env.run(&["delete", "41", "--db"]);
    let after_delete = fs::read(env.db()).unwrap();
    assert!(
        after_add == before && after_delete == before,
        "valueless --db changed persistent records; add: {}; delete: {}",
        diagnostic(&added),
        diagnostic(&deleted)
    );
    for output in [&added, &deleted] {
        rejected(output);
        let error = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            error.contains("--db") && (error.contains("value") || error.contains("file")),
            "failure must explain the missing --db value: {}",
            diagnostic(output)
        );
    }
    for candidate in [
        "data/utsurigi-memory/memory.db",
        "home/.local/share/utsurigi-memory/memory.db",
        "package/memory.db",
        "memory.db",
        "--db",
    ] {
        assert!(
            !env.root.join(candidate).exists(),
            "unexpected database: {candidate}"
        );
    }
}

#[test]
fn selected_non_utf8_path_is_rejected_before_deleting_default_records() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let default = env.root.join("package/memory.db");
    fs::rename(env.db(), &default).unwrap();
    let before = fs::read(&default).unwrap();
    let mut command = env.command(&["--json", "delete", "41"]);
    command.env(
        "UTSURIGI_MEMORY_PATH",
        OsStr::from_bytes(b"private-path-\xff"),
    );
    let output = run(command);
    assert!(
        fs::read(&default).unwrap() == before,
        "invalid PATH changed seeded default records: {}",
        diagnostic(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("UTSURIGI_MEMORY_PATH")
            && error.to_ascii_lowercase().replace('-', "").contains("utf8"),
        "{}",
        diagnostic(&output)
    );
    assert!(!diagnostic(&output).contains("private-path-"));
    assert!(!env.db().exists());
}

#[test]
fn selected_non_utf8_sock_is_rejected_before_contacting_default_daemon() {
    let env = Sandbox::new();
    let listener = UnixListener::bind(env.root.join("cache/utsurigi-memory.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = env.command(&["--json", "status"]);
    command.env(
        "UTSURIGI_MEMORY_SOCK",
        OsStr::from_bytes(b"private-sock-\xff"),
    );
    let child = std::thread::spawn(move || run(command));
    let contacted = loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let _ = writeln!(stream, "{}", serde_json::json!({"pid":73}));
                break true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.is_finished() {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept default daemon connection: {error}"),
        }
    };
    let output = child.join().unwrap();
    assert!(
        !contacted && listener.accept().is_err(),
        "invalid SOCK contacted the default daemon: {}",
        diagnostic(&output)
    );
    assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("UTSURIGI_MEMORY_SOCK")
            && error.to_ascii_lowercase().replace('-', "").contains("utf8"),
        "{}",
        diagnostic(&output)
    );
    assert!(!diagnostic(&output).contains("private-sock-"));
    assert!(!env.db().exists());
}

#[test]
fn control_explicit_db_overrides_non_utf8_path_without_touching_default_records() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let default = env.root.join("package/memory.db");
    fs::copy(env.db(), &default).unwrap();
    let before = fs::read(&default).unwrap();
    let mut command = env.command(&["--db", env.db().to_str().unwrap(), "--json", "delete", "41"]);
    command.env(
        "UTSURIGI_MEMORY_PATH",
        OsStr::from_bytes(b"private-path-\xff"),
    );
    let output = run(command);
    success(&output);
    assert!(output.stderr.is_empty());
    let deleted: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(deleted["deleted"], true);
    assert!(env.json(&["--json", "list"]).as_array().unwrap().is_empty());
    assert!(fs::read(&default).unwrap() == before);
}

#[test]
fn control_unused_non_utf8_sock_and_invalid_idle_allow_info_and_snapshot() {
    let env = Sandbox::new();
    env.legacy(HASH);
    for (index, idle) in [OsStr::new("not-a-number"), OsStr::from_bytes(b"\xff")]
        .into_iter()
        .enumerate()
    {
        let daemon_command = |args: &[&str]| {
            let mut command = env.command(args);
            command
                .env_remove("UTSURIGI_MEMORY_NO_DAEMON")
                .env(
                    "UTSURIGI_MEMORY_SOCK",
                    OsStr::from_bytes(b"private-sock-\xff"),
                )
                .env("UTSURIGI_MEMORY_IDLE_SECS", idle);
            command
        };
        let info = run(daemon_command(&["info"]));
        success(&info);
        assert!(info.stderr.is_empty());
        let text = String::from_utf8_lossy(&info.stdout);
        assert!(text.contains("count: 1") && text.contains(HASH));
        let destination = env.root.join(format!("unused-sock-{index}.db"));
        let output = run(daemon_command(&["snapshot", destination.to_str().unwrap()]));
        success(&output);
        assert!(output.stderr.is_empty());
        let snapshot =
            Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let record: String = snapshot
            .query_row("SELECT text FROM memories WHERE id=41", [], |r| r.get(0))
            .unwrap();
        assert_eq!(record, "古い記録 🦀");
    }
    for name in ["utsurigi-memory.lock", "utsurigi-memory.log"] {
        assert!(!env.root.join("cache").join(name).exists());
    }
    assert!(!env.root.join("sock").exists());
    assert!(!env.root.join("cache/utsurigi-memory.sock").exists());
}

#[test]
fn explicit_db_overrides_non_utf8_path_when_starting_daemon() {
    let logs = [None, Some(OsStr::from_bytes(b"private-path-\xff"))].map(|path| {
        let env = Sandbox::new();
        let database = env.root.join("explicit.db");
        let mut command = env.command(&[
            "--db",
            database.to_str().unwrap(),
            "add",
            "explicit database daemon precedence",
        ]);
        command
            .env_remove("UTSURIGI_MEMORY_NO_DAEMON")
            .env_remove("UTSURIGI_MEMORY_PATH");
        if let Some(value) = path {
            command.env("UTSURIGI_MEMORY_PATH", value);
        }
        let output = run(command);
        let detail = diagnostic(&output);
        env.model_failure(&output, None);
        env.daemon_attempted();
        let log = fs::read_to_string(env.root.join("cache/utsurigi-memory.log")).unwrap();
        eprintln!(
            "PATH {}: {detail}\ndaemon log: {log}",
            if path.is_some() {
                "non-UTF-8"
            } else {
                "unset control"
            }
        );
        assert_eq!(output.status.code(), Some(1), "{detail}");
        for candidate in [
            "explicit.db",
            "package/memory.db",
            "data/utsurigi-memory/memory.db",
            "home/.local/share/utsurigi-memory/memory.db",
            "memory.db",
        ] {
            assert!(
                !env.root.join(candidate).exists(),
                "created {candidate}: {detail}"
            );
        }
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("e5 model unavailable:") && log.contains("e5 model unavailable:"),
            "expected the missing-model error after daemon startup: {detail}"
        );
        for text in [&detail, &log] {
            assert!(
                !text.contains("UTSURIGI_MEMORY_PATH") && !text.contains("private-path-"),
                "unused PATH was validated or echoed: {text}"
            );
        }
        log
    });
    assert_eq!(
        logs[0], logs[1],
        "unused PATH changed the daemon model error"
    );
}
#[test]
fn malformed_flags_leave_existing_and_missing_databases_untouched() {
    let cases = [
        vec!["--hash", "add", "text", "--tag"],
        vec!["--hash", "add", "text", "--tag", "a", "--tag", "b"],
        vec![
            "--hash",
            "add",
            "text",
            "--tag",
            "--created-at",
            "2023-11-15T07:13:20Z",
        ],
        vec!["--hash", "add", "text", "--db", "one", "--db", "two"],
        vec!["--hash", "add", "text", "--unknown"],
        vec!["--hash", "--hash", "add", "text"],
        vec!["--json", "--json", "delete", "41"],
        vec!["delete", "41", "--tag"],
        vec!["delete", "41", "--tag", "a"],
        vec!["--hash", "add", "text", "-k", "1"],
        vec!["--hash", "search", "text", "-k", "bad"],
        vec!["--hash", "search", "text", "-k"],
        vec!["--hash", "search", "text", "-k", "1", "-k", "2"],
        vec!["serve", "--idle", "bad"],
        vec!["serve", "--idle"],
        vec!["list", "--idle", "2"],
        vec!["info", "extra"],
    ];
    for existing in [false, true] {
        let env = Sandbox::new();
        if existing {
            env.legacy(HASH);
        }
        let before = fs::read(env.db()).ok();
        for args in &cases {
            let output = env.run(args);
            assert!(
                !output.status.success(),
                "{args:?}: {}",
                diagnostic(&output)
            );
            assert_eq!(fs::read(env.db()).ok(), before, "{args:?}");
            assert!(!env.root.join("sock").exists());
            assert!(!env.root.join("one").exists());
            assert!(!env.root.join("two").exists());
        }
    }
}

#[test]
fn option_separator_preserves_literal_flag_names_in_memory_text() {
    let env = Sandbox::new();
    success(&env.run(&["--hash", "add", "--", "--json", "--tag", "literal"]));
    let rows = env.json(&["--json", "list"]);
    assert_eq!(rows[0]["text"], "--json --tag literal");
    assert!(rows[0]["tag"].is_null());
}

#[test]
fn info_and_stop_return_json_when_requested() {
    let env = Sandbox::new();
    env.legacy(HASH);
    let info = env.json(&["--json", "info"]);
    assert_eq!(info["count"], 1);
    assert_eq!(info["embedder"], HASH);
    assert_eq!(info["db"], env.db().to_str().unwrap());
    let stop = env.json(&["--json", "stop"]);
    assert_eq!(stop["stopped"], false);
    let listener = UnixListener::bind(env.root.join("sock")).unwrap();
    let (output, request) = socket_request(
        listener,
        env.command(&["stop", "--json"]),
        serde_json::json!({"ok": true}),
    );
    success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["stopped"],
        true
    );
    assert_eq!(request.unwrap()["op"], "stop");
}

#[test]
fn status_and_stop_do_not_create_missing_cache_directories() {
    let env = Sandbox::new();
    let cache = env.root.join("missing-cache");
    for args in [vec!["--json", "status"], vec!["--json", "stop"]] {
        let mut command = env.command(&args);
        command
            .env("UTSURIGI_MEMORY_CACHE", &cache)
            .env_remove("UTSURIGI_MEMORY_SOCK");
        success(&run(command));
        assert!(!cache.exists());
        assert!(!env.db().exists());
    }
}

#[test]
fn model_chunks_with_unpadded_numbers_are_joined_in_numeric_order() {
    let env = Sandbox::new();
    env.model("invalid");
    let dir = env.root.join("package/model/multilingual-e5-small");
    fs::remove_file(dir.join("model.onnx")).unwrap();
    let bytes = b"0123456789!";
    fs::write(
        dir.join("model.onnx.sha256"),
        format!("{:x}", Sha256::digest(bytes)),
    )
    .unwrap();
    for (i, byte) in bytes.iter().enumerate() {
        fs::write(dir.join(format!("model.onnx.part{i}")), [*byte]).unwrap();
    }
    // The bytes deliberately are not ONNX. Reaching the verified cache proves
    // joining passed the checksum before the expected inference-loader failure.
    rejected(&env.run(&["add", "record"]));
    assert_eq!(fs::read(env.root.join("cache/model.onnx")).unwrap(), bytes);
    assert!(!env.db().exists());
}

// Bind at the expected path so these tests prove path precedence without relying
// on status to create directories. The separate absent-daemon test checks no writes.
fn status_at(command: Command, directory: &Path) -> Output {
    fs::create_dir_all(directory).unwrap();
    let listener = UnixListener::bind(directory.join("utsurigi-memory.sock")).unwrap();
    let (output, request) = socket_request(
        listener,
        command,
        serde_json::json!({"running": false, "pid": 73}),
    );
    assert_eq!(request.unwrap()["op"], "status");
    success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["pid"],
        73
    );
    output
}

#[test]
fn malformed_or_duplicate_model_part_numbers_fail_before_database_creation() {
    for names in [["part1", "part01"], ["part1", "partbad"]] {
        let env = Sandbox::new();
        env.model("invalid");
        let model = env.root.join("package/model/multilingual-e5-small");
        fs::remove_file(model.join("model.onnx")).unwrap();
        for name in names {
            fs::write(model.join(format!("model.onnx.{name}")), b"invalid model").unwrap();
        }
        let output = env.run(&["add", "must not be stored"]);
        assert!(!output.status.success(), "{}", diagnostic(&output));
        let detail = String::from_utf8_lossy(&output.stderr);
        assert!(detail.contains("model chunk"), "{detail}");
        assert!(!env.db().exists());
    }
}
