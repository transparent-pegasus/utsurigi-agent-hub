//! utsurigi-memory — minimal single-file vector memory DB.
//! Storage: one SQLite file (vectors = little-endian f32 BLOBs). Search: brute-force cosine (self-written).
//! Embeddings: multilingual-e5-small via fastembed (ONNX), held by a background daemon
//! (`utsurigi-memory serve`, Unix socket, exits when idle). Explicit --hash: char n-gram hashing.

use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HASH_DIM: usize = 1024;
const E5_DIM: usize = 384;
const DEFAULT_IDLE_SECS: u64 = 600;

// ---------------------------------------------------------------- paths / config
fn memory_env(suffix: &str) -> Option<String> {
    let name = format!("UTSURIGI_MEMORY_{suffix}");
    match std::env::var(&name) {
        Ok(value) => Some(value).filter(|s| !s.is_empty()),
        Err(std::env::VarError::NotUnicode(_)) if matches!(suffix, "PATH" | "SOCK") => {
            fail(&format!("{name} must be UTF-8"))
        }
        Err(_) => None,
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("UTSURIGI_MEMORY_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_DATA_HOME")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| user_home().join(".local/share"))
                .join("utsurigi-memory")
        })
}
fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            fail("HOME is not set; set UTSURIGI_MEMORY_HOME and UTSURIGI_MEMORY_CACHE")
        })
}
/// Runtime directory for sockets, locks, logs, and the joined model.
fn cache_dir() -> PathBuf {
    std::env::var_os("UTSURIGI_MEMORY_CACHE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| user_home().join(".cache"))
                .join("utsurigi-memory")
        })
}
fn sock_path() -> Result<PathBuf, String> {
    let path = memory_env("SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(|| cache_dir().join("utsurigi-memory.sock"));
    SocketAddr::from_pathname(&path).map_err(|e| {
        format!(
            "invalid socket path ({} bytes): {e}; set UTSURIGI_MEMORY_SOCK to a shorter path",
            path.as_os_str().as_encoded_bytes().len()
        )
    })?;
    Ok(path)
}
fn idle_secs() -> u64 {
    memory_env("IDLE_SECS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_IDLE_SECS)
}
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// ---------------------------------------------------------------- embedding
enum Embedder {
    E5(Box<fastembed::TextEmbedding>),
    Hash,
}

impl Embedder {
    fn name(&self) -> &'static str {
        match self {
            Embedder::E5(_) => "multilingual-e5-small",
            Embedder::Hash => "char-ngram-hash-1024",
        }
    }

    fn load(force_hash: bool) -> Result<Embedder, String> {
        if force_hash {
            return Ok(Embedder::Hash);
        }
        load_e5(&home_dir().join("model/multilingual-e5-small"))
            .map(|m| Embedder::E5(Box::new(m)))
            .map_err(|e| format!("e5 model unavailable: {e}; install the verified model or explicitly use --hash"))
    }

    /// is_query selects the e5 prefix ("query: " vs "passage: ").
    fn embed(&mut self, text: &str, is_query: bool) -> Result<Vec<f32>, String> {
        let mut v = match self {
            Embedder::E5(m) => {
                let p = if is_query { "query: " } else { "passage: " };
                m.embed(vec![format!("{p}{text}")], None)
                    .map_err(|e| e.to_string())?
                    .pop()
                    .ok_or("model returned no embedding")?
            }
            Embedder::Hash => hash_embed(text),
        };
        validate_vector(&v, embedding_dim(self.name())?)?;
        normalize(&mut v);
        Ok(v)
    }
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// Accept a complete model or ordered model.onnx.partNN chunks; verify every read.
fn model_bytes(dir: &Path) -> Result<Vec<u8>, String> {
    let checksum = std::fs::read_to_string(dir.join("model.onnx.sha256"))
        .map_err(|e| format!("model checksum: {e}"))?;
    let want = checksum.split_whitespace().next().unwrap_or("");
    if want.len() != 64 || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid model checksum; expected SHA256".into());
    }
    let verify = |bytes: Vec<u8>| {
        let got = sha256_hex(&bytes);
        if !want.eq_ignore_ascii_case(&got) {
            return Err(format!(
                "model checksum mismatch (expected {want}, got {got})"
            ));
        }
        Ok(bytes)
    };
    if dir.join("model.onnx").exists() {
        return verify(std::fs::read(dir.join("model.onnx")).map_err(|e| e.to_string())?);
    }
    let mut parts: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("model.onnx.part")
        })
        .collect();
    if parts.is_empty() {
        return Err("no model.onnx".into());
    }
    let mut numbered = parts
        .drain(..)
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            let suffix = name.strip_prefix("model.onnx.part").unwrap();
            let number = suffix
                .parse::<u64>()
                .ok()
                .filter(|_| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
                .ok_or_else(|| format!("invalid model chunk: {}", path.display()))?;
            Ok((number, path))
        })
        .collect::<Result<Vec<_>, String>>()?;
    numbered.sort_by_key(|(number, _)| *number);
    if numbered.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("duplicate model chunk number".into());
    }
    let cached = cache_dir().join("model.onnx");
    if cached.exists() {
        return verify(std::fs::read(&cached).map_err(|e| e.to_string())?);
    }
    let mut buf = Vec::new();
    for (_, p) in &numbered {
        buf.extend(std::fs::read(p).map_err(|e| e.to_string())?);
    }
    let buf = verify(buf)?;
    std::fs::create_dir_all(cache_dir()).map_err(|e| format!("cache directory: {e}"))?;
    let tmp = cached.with_extension(format!("onnx.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &buf).map_err(|e| format!("cache model: {e}"))?;
    std::fs::rename(&tmp, &cached).map_err(|e| format!("cache model: {e}"))?;
    Ok(buf)
}

fn load_e5(dir: &Path) -> Result<fastembed::TextEmbedding, String> {
    use fastembed::*;
    let read = |n: &str| std::fs::read(dir.join(n)).map_err(|e| format!("{n}: {e}"));
    let tok = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model = UserDefinedEmbeddingModel::new(model_bytes(dir)?, tok).with_pooling(Pooling::Mean);
    TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::new().with_max_length(512),
    )
    .map_err(|e| e.to_string())
}

/// Explicit hash mode: character 1-3 grams (FNV-1a) into a fixed-size vector.
fn hash_embed(text: &str) -> Vec<f32> {
    let chars: Vec<char> = text
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut v = vec![0f32; HASH_DIM];
    for n in 1..=3 {
        for w in chars.windows(n) {
            let mut h: u64 = 0xcbf29ce484222325;
            for c in w.iter().collect::<String>().bytes() {
                h = (h ^ c as u64).wrapping_mul(0x100000001b3);
            }
            let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
            v[(h % HASH_DIM as u64) as usize] += sign * n as f32;
        }
    }
    v
}

// ---------------------------------------------------------------- vector math
fn embedding_dim(name: &str) -> Result<usize, String> {
    match name {
        "multilingual-e5-small" => Ok(E5_DIM),
        "char-ngram-hash-1024" => Ok(HASH_DIM),
        _ => Err(format!("unknown embedder: {name}")),
    }
}

fn validate_vector(v: &[f32], dim: usize) -> Result<(), String> {
    if v.len() != dim {
        return Err(format!(
            "invalid vector dimension: expected {dim}, got {}",
            v.len()
        ));
    }
    if v.iter().any(|x| !x.is_finite()) {
        return Err("invalid vector: non-finite value".into());
    }
    Ok(())
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x = (f64::from(*x) / n) as f32);
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot / (na.sqrt() * nb.sqrt())) as f32
    }
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8], dim: usize) -> Result<Vec<f32>, String> {
    if b.len() != dim * 4 {
        return Err(format!(
            "invalid vector byte length: expected {}, got {}",
            dim * 4,
            b.len()
        ));
    }
    let v = b
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect::<Vec<_>>();
    validate_vector(&v, dim)?;
    Ok(v)
}

// ---------------------------------------------------------------- daemon (holds the model)
// Protocol: one JSON request line -> one JSON response line per connection.
//   {"op":"embed","text":..,"query":bool} -> {"embedder":..,"vec":[..]}
//   {"op":"status"} -> {...}      {"op":"stop"} -> {"ok":true}
fn serve(idle: u64) -> Result<(), String> {
    let sock = sock_path()?;
    if UnixStream::connect(&sock).is_ok() {
        eprintln!(
            "[utsurigi-memory] daemon already running at {}",
            sock.display()
        );
        return Ok(());
    }
    let t0 = Instant::now();
    let mut emb = Embedder::load(false)?;
    let load_ms = t0.elapsed().as_millis();
    if let Some(parent) = sock.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("socket directory: {e}"))?;
    }
    std::fs::remove_file(&sock).ok();
    let listener = UnixListener::bind(&sock).map_err(|e| format!("bind socket: {e}"))?;
    eprintln!(
        "[utsurigi-memory] serving {} pid={} embedder={} load={}ms idle={}s",
        sock.display(),
        std::process::id(),
        emb.name(),
        load_ms,
        idle
    );

    // idle watchdog: exit (and unload the model) after `idle` seconds without requests
    let last = Arc::new(AtomicU64::new(now_secs()));
    {
        let (last, sock) = (last.clone(), sock.clone());
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(1));
            if now_secs().saturating_sub(last.load(Ordering::Relaxed)) >= idle {
                eprintln!("[utsurigi-memory] idle {idle}s, exiting");
                std::fs::remove_file(&sock).ok();
                std::process::exit(0);
            }
        });
    }
    let started = now_secs();
    for conn in listener.incoming() {
        let Ok(mut s) = conn else { continue };
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let mut line = String::new();
        if BufReader::new(&s).read_line(&mut line).is_err() {
            continue;
        }
        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let resp = match req["op"].as_str() {
            Some("embed") => {
                // Start the request with a fresh idle window; status polls do not extend it.
                last.store(now_secs(), Ordering::Relaxed);
                let text = req["text"].as_str().unwrap_or("");
                match emb.embed(text, req["query"].as_bool().unwrap_or(false)) {
                    Ok(v) => json!({"embedder": emb.name(), "vec": v}),
                    Err(e) => json!({"error": e}),
                }
            }
            Some("status") => {
                let n = now_secs();
                json!({"pid": std::process::id(), "embedder": emb.name(), "socket": sock.display().to_string(),
                       "uptime_s": n - started, "load_ms": load_ms as u64, "idle_timeout_s": idle,
                       "idle_left_s": idle.saturating_sub(n - last.load(Ordering::Relaxed))})
            }
            Some("stop") => {
                writeln!(s, "{}", json!({"ok": true})).ok();
                std::fs::remove_file(&sock).ok();
                std::process::exit(0);
            }
            _ => json!({"error": "bad request"}),
        };
        if req["op"] == "embed" {
            // Leave a full idle window after inference, which may itself take time.
            last.store(now_secs(), Ordering::Relaxed);
        }
        writeln!(s, "{resp}").ok();
    }
    Ok(())
}

fn request(req: &Value) -> Option<Value> {
    let sock = sock_path().unwrap_or_else(|e| fail(&e));
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(120))).ok();
    writeln!(s, "{req}").ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    serde_json::from_str(out.trim()).ok()
}

/// Connect to the daemon, spawning it (detached, guarded by a lockfile) if needed.
fn ensure_daemon() -> Result<(), String> {
    let sock = sock_path()?;
    if UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(cache_dir()).map_err(|e| format!("cache directory: {e}"))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(cache_dir().join("utsurigi-memory.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?; // released when `lock` drops
    if UnixStream::connect(&sock).is_ok() {
        return Ok(()); // someone else spawned it while we waited for the lock
    }
    use std::os::unix::process::CommandExt;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cache_dir().join("utsurigi-memory.log"))
        .map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("serve")
        // The embedding daemon never accesses the database.
        .env_remove("UTSURIGI_MEMORY_PATH")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| format!("daemon log: {e}"))?)
        .stderr(log)
        .process_group(0) // detach from our process group
        .spawn()
        .map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(180) {
        if UnixStream::connect(&sock).is_ok() {
            return Ok(());
        }
        if let Ok(Some(st)) = child.try_wait() {
            let log = cache_dir().join("utsurigi-memory.log");
            let detail = std::fs::read_to_string(&log).unwrap_or_default();
            return Err(format!(
                "daemon exited ({st}): {}; see {}",
                detail
                    .lines()
                    .last()
                    .unwrap_or("model initialization failed"),
                log.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("daemon did not become ready".into())
}

/// Embed via daemon; returns (embedder name, vector).
fn embed(text: &str, is_query: bool, force_hash: bool) -> Result<(String, Vec<f32>), String> {
    if force_hash || std::env::var_os("UTSURIGI_MEMORY_NO_DAEMON").is_some() {
        let mut e = Embedder::load(force_hash)?;
        return Ok((e.name().into(), e.embed(text, is_query)?));
    }
    ensure_daemon()?;
    let r = request(&json!({"op": "embed", "text": text, "query": is_query}))
        .ok_or("no response from daemon")?;
    if let Some(e) = r["error"].as_str() {
        return Err(e.into());
    }
    let name = r["embedder"]
        .as_str()
        .ok_or("daemon response has no embedder")?;
    if name != "multilingual-e5-small" {
        return Err(format!(
            "unexpected daemon embedder: {name}; use --hash explicitly for hash databases"
        ));
    }
    let v = r["vec"]
        .as_array()
        .ok_or("invalid daemon vector")?
        .iter()
        .map(|x| {
            x.as_f64()
                .map(|x| x as f32)
                .ok_or("invalid daemon vector value")
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_vector(&v, embedding_dim(name)?)?;
    Ok((name.into(), v))
}

// ---------------------------------------------------------------- storage
struct Db {
    conn: Connection,
}

struct Row {
    id: i64,
    text: String,
    tag: Option<String>,
    created_at: i64,
}

impl Db {
    fn open(path: &Path) -> Result<Db, String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("database directory: {e}"))?;
        }
        let conn = Connection::open(path).map_err(|e| format!("open database: {e}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE IF NOT EXISTS memories(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               text TEXT NOT NULL, tag TEXT,
               created_at INTEGER NOT NULL,   -- unix seconds (UTC)
               vec BLOB NOT NULL);            -- little-endian f32, L2-normalized",
        )
        .map_err(|e| format!("init schema: {e}"))?;
        Ok(Db { conn })
    }

    fn open_existing(path: &Path) -> Result<Db, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| format!("open database: {e}"))?;
        Ok(Db { conn })
    }

    fn open_read_only(path: &Path) -> Result<Db, String> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open database read-only: {e}"))?;
        Ok(Db { conn })
    }

    fn meta(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()
            .map_err(|e| format!("read metadata: {e}"))
    }

    /// The first embedder used is pinned so vectors in one file stay comparable.
    fn pin_embedder(&self, name: &str) -> Result<(), String> {
        if self.meta("embedder")?.is_none() {
            self.conn
                .execute("INSERT INTO meta VALUES('embedder', ?)", [name])
                .map_err(|e| format!("pin embedder: {e}"))?;
            return Ok(());
        }
        self.check_embedder(name)
    }

    fn check_embedder(&self, name: &str) -> Result<(), String> {
        match self.meta("embedder")? {
            Some(e) if e != name => Err(format!(
                "db was built with '{e}', current embedder is '{name}'"
            )),
            Some(_) => Ok(()),
            None => Err("database has no pinned embedder".into()),
        }
    }

    fn add(
        &self,
        text: &str,
        tag: Option<&str>,
        v: &[f32],
        created_at: i64,
    ) -> Result<i64, String> {
        self.conn
            .execute(
                "INSERT INTO memories(text, tag, created_at, vec) VALUES(?,?,?,?)",
                params![text, tag, created_at, to_blob(v)],
            )
            .map_err(|e| format!("insert memory: {e}"))?;
        Ok(self.conn.last_insert_rowid())
    }

    fn all(&self, with_vec: bool) -> Result<Vec<(Row, Vec<f32>)>, String> {
        let dim = if with_vec {
            Some(embedding_dim(
                &self
                    .meta("embedder")?
                    .ok_or("database has no pinned embedder")?,
            )?)
        } else {
            None
        };
        let sql = if with_vec {
            "SELECT id, text, tag, created_at, vec FROM memories ORDER BY id"
        } else {
            "SELECT id, text, tag, created_at, x'' FROM memories ORDER BY id"
        };
        let mut st = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                let blob: Vec<u8> = r.get(4)?;
                Ok((
                    Row {
                        id: r.get(0)?,
                        text: r.get(1)?,
                        tag: r.get(2)?,
                        created_at: r.get(3)?,
                    },
                    blob,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| {
            let (row, blob) = row.map_err(|e| format!("read memory: {e}"))?;
            let vector = match dim {
                Some(dim) => {
                    from_blob(&blob, dim).map_err(|e| format!("memory #{}: {e}", row.id))?
                }
                None => Vec::new(),
            };
            Ok((row, vector))
        })
        .collect()
    }

    fn count(&self) -> Result<i64, String> {
        self.conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .map_err(|e| format!("count memories: {e}"))
    }

    fn delete(&self, id: i64) -> Result<bool, String> {
        self.conn
            .execute("DELETE FROM memories WHERE id=?", [id])
            .map(|changed| changed > 0)
            .map_err(|e| format!("delete memory: {e}"))
    }
}

fn snapshot(source: &Path, destination: &Path) -> Result<(), String> {
    let db = Db::open_read_only(source).map_err(|e| format!("snapshot source: {e}"))?;
    let destination =
        std::path::absolute(destination).map_err(|e| format!("snapshot destination: {e}"))?;
    let filename = destination
        .to_str()
        .ok_or("snapshot destination must be UTF-8")?;
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("snapshot directory: {e}"))?;
    }
    // VACUUM INTO accepts an existing empty file; reserve it exclusively first.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&destination)
        .map_err(|e| format!("create snapshot destination: {e}"))?;
    if let Err(e) = db.conn.execute("VACUUM INTO ?1", [filename]) {
        std::fs::remove_file(&destination)
            .map_err(|cleanup| format!("snapshot: {e}; remove incomplete snapshot: {cleanup}"))?;
        return Err(format!("snapshot: {e}"));
    }
    Ok(())
}

// ---------------------------------------------------------------- CLI
fn utc(ts: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or_else(|| fail("invalid stored timestamp"))
}

fn row_json(r: &Row, score: Option<f32>) -> Value {
    let mut j = json!({
        "id": r.id,
        "text": r.text,
        "tag": r.tag,
        "created_at": utc(r.created_at).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    if let Some(s) = score {
        j["score"] = json!((s as f64 * 10000.0).round() / 10000.0);
    }
    j
}

fn row_line(r: &Row, score: Option<f32>) -> String {
    let s = score.map(|s| format!("{s:.3}  ")).unwrap_or_default();
    let t = r
        .tag
        .as_ref()
        .map(|t| format!(" [{t}]"))
        .unwrap_or_default();
    format!(
        "{s}#{}  {}{}  {}",
        r.id,
        utc(r.created_at).format("%Y-%m-%d %H:%M UTC"),
        t,
        r.text.replace('\n', " ")
    )
}

const USAGE: &str = "usage: utsurigi-memory [--db FILE] [--json] [--hash] <command>
  add \"text\" [--tag T] [--created-at RFC3339]  store a memory, prints id
  search \"query\" [-k N]    top-N by cosine similarity (default 5)
  list                     all memories
  delete <id>              remove a memory
  info                     db path, embedder, count
  snapshot DESTINATION     consistent SQLite export; refuses existing destinations
  serve [--idle SECS]      run the embedding daemon (auto-started by add/search)
  status                   daemon status
  stop                     stop the daemon (unloads the model)
env: UTSURIGI_MEMORY_HOME (data/model dir; default $XDG_DATA_HOME/utsurigi-memory or ~/.local/share/utsurigi-memory),
     UTSURIGI_MEMORY_PATH (db file; default memory.db in data/model dir), UTSURIGI_MEMORY_IDLE_SECS (default 600),
     UTSURIGI_MEMORY_SOCK, UTSURIGI_MEMORY_CACHE (default $XDG_CACHE_HOME/utsurigi-memory or ~/.cache/utsurigi-memory),
     UTSURIGI_MEMORY_NO_DAEMON (present, even empty: embed in-process).
Other empty UTSURIGI_MEMORY_* values and empty XDG values are unset.
Selected PATH and SOCK values must be UTF-8. Socket paths must fit the platform byte limit.
Invalid selected IDLE_SECS uses 600; serve --idle SECS takes precedence.
--db FILE has highest precedence; FILE is required and must not be empty.
--created-at is add-only; default now, stored as UTC Unix seconds (fractional seconds discarded).
info/list/search/snapshot open existing databases read-only; missing sources are not created.
delete opens existing databases for writes; it creates no databases or parent directories.";

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

/// Validate the entire invocation before resolving paths or opening a database.
fn parse_args() -> (Vec<String>, std::collections::BTreeMap<String, String>) {
    let mut positional = Vec::new();
    let mut options = std::collections::BTreeMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            positional.extend(args);
            break;
        }
        if !arg.starts_with('-') {
            positional.push(arg);
            continue;
        }
        if options.contains_key(&arg) {
            fail(&format!("{arg} may only be provided once"));
        }
        let value = match arg.as_str() {
            "--json" | "--hash" => String::new(),
            "--db" | "--tag" | "--created-at" | "-k" | "--idle" => args
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .unwrap_or_else(|| {
                    if arg == "--created-at" {
                        fail("--created-at requires RFC3339");
                    }
                    fail(&format!("{arg} requires a nonempty value"))
                }),
            _ => fail(&format!("unknown option: {arg}")),
        };
        options.insert(arg, value);
    }
    let cmd = positional.first().map(String::as_str).unwrap_or("");
    for (flag, command) in [
        ("--tag", "add"),
        ("--created-at", "add"),
        ("-k", "search"),
        ("--idle", "serve"),
    ] {
        if options.contains_key(flag) && cmd != command {
            fail(&format!("{flag} is only valid with {command}"));
        }
    }
    let valid = match cmd {
        "add" | "search" => positional.len() > 1 && !positional[1..].join(" ").trim().is_empty(),
        "delete" | "snapshot" => positional.len() == 2 && !positional[1].is_empty(),
        "info" | "list" | "serve" | "status" | "stop" => positional.len() == 1,
        _ => false,
    };
    if !valid {
        fail(USAGE);
    }
    (positional, options)
}

fn main() {
    let (args, mut options) = parse_args();
    let json_out = options.remove("--json").is_some();
    let force_hash = options.remove("--hash").is_some();
    let tag = options.remove("--tag");
    let created_at = options.remove("--created-at").map(|value| {
        chrono::DateTime::parse_from_rfc3339(&value)
            .unwrap_or_else(|e| fail(&format!("--created-at requires RFC3339: {e}")))
            .timestamp()
    });
    let k: usize = options
        .remove("-k")
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| fail("-k requires a nonnegative integer"))
        })
        .unwrap_or(5);
    let idle: u64 = options
        .remove("--idle")
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| fail("--idle requires a nonnegative integer"))
        })
        .unwrap_or_else(idle_secs);
    let db_path = options
        .remove("--db")
        .map(PathBuf::from)
        .or_else(|| memory_env("PATH").map(PathBuf::from))
        .unwrap_or_else(|| home_dir().join("memory.db"));
    let cmd = args[0].as_str();
    let arg = args[1..].join(" ");

    // daemon commands (no db)
    match cmd {
        "serve" => return serve(idle).unwrap_or_else(|e| fail(&e)),
        "status" => {
            match request(&json!({"op": "status"})) {
                Some(r) if json_out => println!("{r}"),
                Some(r) => println!("running pid={} embedder={} uptime={}s idle_timeout={}s idle_left={}s socket={}",
                    r["pid"], r["embedder"].as_str().unwrap_or("?"), r["uptime_s"], r["idle_timeout_s"], r["idle_left_s"],
                    r["socket"].as_str().unwrap_or("?")),
                None if json_out => println!("{}", json!({"running": false})),
                None => println!("not running"),
            }
            return;
        }
        "stop" => {
            let ok = request(&json!({"op": "stop"})).is_some();
            if json_out {
                println!("{}", json!({"stopped": ok}));
            } else {
                println!("{}", if ok { "stopped" } else { "not running" });
            }
            return;
        }
        "snapshot" => {
            if args.len() != 2 || arg.is_empty() {
                fail(USAGE);
            }
            snapshot(&db_path, Path::new(&arg)).unwrap_or_else(|e| fail(&e));
            if json_out {
                println!("{}", json!({"snapshot": arg}));
            } else {
                println!("{arg}");
            }
            return;
        }
        _ => {}
    }

    match cmd {
        "add" | "search" if arg.trim().is_empty() => fail(USAGE),
        "add" => {
            let (name, v) = embed(&arg, false, force_hash).unwrap_or_else(|e| fail(&e));
            let db = Db::open(&db_path).unwrap_or_else(|e| fail(&e));
            // a db that already has vectors must keep using the same embedder
            db.pin_embedder(&name).unwrap_or_else(|e| fail(&e));
            let id = db
                .add(
                    &arg,
                    tag.as_deref(),
                    &v,
                    created_at.unwrap_or_else(|| Utc::now().timestamp()),
                )
                .unwrap_or_else(|e| fail(&e));
            if json_out {
                println!("{}", json!({"id": id, "embedder": name}));
            } else {
                println!("{id}");
            }
        }
        "search" => {
            let db = Db::open_read_only(&db_path).unwrap_or_else(|e| fail(&e));
            let (name, v) = embed(&arg, true, force_hash).unwrap_or_else(|e| fail(&e));
            db.check_embedder(&name).unwrap_or_else(|e| fail(&e));
            let mut hits: Vec<(f32, Row)> = db
                .all(true)
                .unwrap_or_else(|e| fail(&e))
                .into_iter()
                .map(|(r, x)| (cosine(&v, &x), r))
                .collect();
            hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            hits.truncate(k);
            if json_out {
                let a: Vec<_> = hits.iter().map(|(s, r)| row_json(r, Some(*s))).collect();
                println!("{}", serde_json::to_string(&a).unwrap());
            } else if hits.is_empty() {
                println!("(no memories)");
            } else {
                hits.iter()
                    .for_each(|(s, r)| println!("{}", row_line(r, Some(*s))));
            }
        }
        "list" => {
            let db = Db::open_read_only(&db_path).unwrap_or_else(|e| fail(&e));
            let rows = db.all(false).unwrap_or_else(|e| fail(&e));
            let mut stdout = std::io::stdout().lock();
            let result = if json_out {
                let a: Vec<_> = rows.iter().map(|(r, _)| row_json(r, None)).collect();
                writeln!(stdout, "{}", serde_json::to_string(&a).unwrap())
            } else if rows.is_empty() {
                writeln!(stdout, "(no memories)")
            } else {
                rows.iter()
                    .try_for_each(|(r, _)| writeln!(stdout, "{}", row_line(r, None)))
            };
            if let Err(e) = result.and_then(|()| stdout.flush()) {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    fail(&format!("stdout: {e}"));
                }
            }
        }
        "delete" => {
            let id: i64 = arg
                .trim()
                .trim_start_matches('#')
                .parse()
                .unwrap_or_else(|_| fail(USAGE));
            let db = Db::open_existing(&db_path).unwrap_or_else(|e| fail(&e));
            let ok = db.delete(id).unwrap_or_else(|e| fail(&e));
            if json_out {
                println!("{}", json!({"deleted": ok, "id": id}));
            } else {
                println!(
                    "{}",
                    if ok {
                        format!("deleted #{id}")
                    } else {
                        format!("not found #{id}")
                    }
                );
            }
            if !ok {
                std::process::exit(1);
            }
        }
        "info" => {
            let db = Db::open_read_only(&db_path).unwrap_or_else(|e| fail(&e));
            let n = db.count().unwrap_or_else(|e| fail(&e));
            let e = db
                .meta("embedder")
                .unwrap_or_else(|e| fail(&e))
                .unwrap_or_else(|| "(unset)".into());
            if json_out {
                println!("{}", json!({"db": db_path, "embedder": e, "count": n}));
            } else {
                println!("db: {}\nembedder: {e}\ncount: {n}", db_path.display());
            }
        }
        _ => fail(USAGE),
    }
}
