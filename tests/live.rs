#[allow(dead_code)]
mod common;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

const HUB: &str = "default";
const TASK: &[u8] = "live task 日本語 plaintext-7c1e\n".as_bytes();
const RULES: &[u8] = b"live rules plaintext-4d2a: keep this longer rule body\n";
const WRONG_PASSWORD: &str = "synthetic-WRONG-secret-5b6c";
const NOT_LISTED: &str =
    "This machine is not listed for the Bot; add status.machine to its registry machines";
const BACKWARDS: &str = "Live list revision went backwards; local files were left unchanged";
const UNDECRYPTABLE: &str = "Live list cannot be decrypted with this repository password; delete uah-live/ from storage and define again";
const SECONDARY: &str = "Secondary runtime: run live check; the primary runtime backs up Bot data";
const REQUEST: &[u8] = "request from beta 日本語 plaintext-2f9a\n".as_bytes();
const KNOWLEDGE: &[u8] = b"hub knowledge from alpha plaintext-5e7d\n";
const PRIVATE_TARGET: &str = "Live requests require a shared target Bot";
const NO_TASK_DIRECTORY: &str = "Target Bot accepts no live requests";
// MAX_BYTES - 40: the largest plaintext whose sealed object fits MAX_BYTES.
const REQUEST_LIMIT: usize = (1 << 20) - 40;

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn machine_hash(id: &str) -> String {
    sha256_hex(
        ["utsurigi-agent-hub machine v1", HUB, id]
            .join("\0")
            .as_bytes(),
    )
}

fn bot_key() -> String {
    sha256_hex(
        ["utsurigi-agent-hub live v1", HUB, "alpha"]
            .join("\0")
            .as_bytes(),
    )
}

// Live roots must lie inside data.include, so every machine includes them.
fn configure(bot: &mut Bot, machine: Option<&str>) {
    if let Some(id) = machine {
        bot.config["machine"] = json!({ "id": id });
    }
    bot.config["data"]["include"] = json!(["notes", "tasks", "RULES.md"]);
    bot.save();
    fs::create_dir_all(bot.path("owned").join("tasks")).unwrap();
}

fn primary() -> Fixture {
    let mut fixture = Fixture::new();
    configure(&mut fixture.alpha, Some("machine-1"));
    write(fixture.alpha.path("owned").join("RULES.md"), RULES);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture
}

// Same Bot ID and repositories as the primary; its own machine, runtime, and paths.
fn machine(fixture: &Fixture, runtime: &str, id: Option<&str>) -> Bot {
    let mut bot = fixture.bot("alpha", runtime);
    configure(&mut bot, id);
    bot
}

fn machines() -> (Fixture, Bot) {
    let fixture = primary();
    let secondary = machine(&fixture, "alpha-2", Some("machine-2"));
    register(&fixture, &secondary);
    (fixture, secondary)
}

// The primary lists the secondary's status.machine in the Bot's registry machines.
fn register(fixture: &Fixture, secondary: &Bot) {
    let hash = status(fixture, secondary)["machine"]
        .as_str()
        .expect("status must report the machine hash")
        .to_owned();
    let primary = &fixture.alpha;
    fixture.author_current(primary);
    let path = primary.path("shared").join("registry.yaml");
    let mut registry: Value = serde_saphyr::from_slice(&fs::read(&path).unwrap()).unwrap();
    let entry = registry["bots"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["id"] == "alpha")
        .unwrap();
    entry["machines"] = json!([hash]);
    fs::write(&path, serde_json::to_vec_pretty(&registry).unwrap()).unwrap();
    fixture.ok(primary, &["registry", "publish"]);
}

fn status(fixture: &Fixture, bot: &Bot) -> Value {
    serde_json::from_slice(&fixture.ok(bot, &["status"]).stdout).expect("status must emit JSON")
}

fn json_line(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "live prints one JSON line: {}",
        text(output)
    );
    serde_json::from_str(lines[0]).unwrap_or_else(|_| panic!("live prints JSON: {}", text(output)))
}

fn live(fixture: &Fixture, bot: &Bot, args: &[&str]) -> Value {
    let args: Vec<_> = ["live"].into_iter().chain(args.iter().copied()).collect();
    json_line(&fixture.ok(bot, &args))
}

// A rejection proves nothing while the live subcommand itself is missing.
fn rejected(output: &Output) -> String {
    failure(output);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(!stderr.contains("unrecognized subcommand"), "{stderr}");
    stderr
}

fn define(fixture: &Fixture, bot: &Bot, args: &[&str]) -> u64 {
    let args: Vec<_> = ["define"].into_iter().chain(args.iter().copied()).collect();
    let report = live(fixture, bot, &args);
    let revision = report["revision"]
        .as_u64()
        .unwrap_or_else(|| panic!("{report}"));
    assert_eq!(report, json!({"defined": true, "revision": revision}));
    assert_eq!(live_list(bot)["revision"], revision);
    revision
}

// Move next_check into the past, as status.json tests move next_due.
fn make_due(bot: &Bot) {
    let path = bot.path("state").join("live.json");
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    let mut state: Value = serde_json::from_slice(&bytes).expect("live.json must be JSON");
    rewind(&mut state, Utc::now().timestamp() - 1);
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn rewind(value: &mut Value, past: i64) {
    match value {
        Value::Object(map) => {
            for (key, slot) in map.iter_mut() {
                if key == "next_check" && slot.is_number() {
                    *slot = json!(past);
                } else if key == "next_check" && slot.is_string() {
                    let time = DateTime::from_timestamp(past, 0).unwrap();
                    *slot = json!(time.to_rfc3339_opts(SecondsFormat::Secs, true));
                } else {
                    rewind(slot, past);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| rewind(item, past)),
        _ => (),
    }
}

struct Checked {
    report: Value,
    started: i64,
    finished: i64,
}

impl Checked {
    fn paths(&self, key: &str) -> Vec<String> {
        let mut paths: Vec<String> = self.report[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| {
                path.as_str()
                    .unwrap_or_else(|| panic!("{key} lists owned-relative paths: {}", self.report))
                    .to_owned()
            })
            .collect();
        paths.sort();
        paths
    }

    fn conflicted(&self) -> bool {
        !self.report["conflicts"].as_array().unwrap().is_empty()
    }

    fn unchanged(&self) -> bool {
        ["pulled", "pushed", "conflicts"]
            .into_iter()
            .all(|key| self.report[key].as_array().unwrap().is_empty())
    }

    // A push or pull is a change, so the next check follows base_seconds ±10%.
    fn assert_interval(&self, base: i64) {
        let next = next_check(&self.report);
        let range = self.started + base * 9 / 10 - 1..=self.finished + base * 11 / 10 + 1;
        assert!(range.contains(&next), "{}", self.report);
    }
}

fn next_check(report: &Value) -> i64 {
    let text = report["next_check"]
        .as_str()
        .unwrap_or_else(|| panic!("{report}"));
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%SZ")
        .unwrap_or_else(|_| panic!("next_check must be YYYY-MM-DDTHH:MM:SSZ: {text}"))
        .and_utc()
        .timestamp()
}

fn check(fixture: &Fixture, bot: &Bot) -> Checked {
    make_due(bot);
    let started = Utc::now().timestamp();
    let report = live(fixture, bot, &["check"]);
    let finished = Utc::now().timestamp();
    assert_ne!(
        report,
        json!({"skipped": true}),
        "the check stayed not due: live.json next_check was not moved"
    );
    let keys: BTreeSet<_> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "checked",
            "conflicts",
            "hub_changed",
            "next_check",
            "pulled",
            "pushed"
        ]),
        "{report}"
    );
    assert_eq!(report["checked"], true);
    assert!(report["hub_changed"].is_boolean(), "{report}");
    for key in ["pulled", "pushed", "conflicts"] {
        assert!(report[key].is_array(), "{report}");
    }
    assert!(next_check(&report) > started, "{report}");
    Checked {
        report,
        started,
        finished,
    }
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, relative: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        let entries = match fs::read_dir(root.join(relative)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("{}: {error}", root.join(relative).display()),
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = relative.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, files);
            } else {
                let bytes = fs::read(entry.path()).unwrap();
                files.insert(path.to_str().unwrap().to_owned(), bytes);
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, Path::new(""), &mut files);
    files
}

// Every live byte in both repositories, keyed by repository and object key.
fn remote(bot: &Bot) -> BTreeMap<String, Vec<u8>> {
    ["bot", "shared"]
        .into_iter()
        .flat_map(|repository| {
            files(&Path::new(bot.repository(repository)).join("uah-live"))
                .into_iter()
                .map(move |(key, bytes)| (format!("{repository}/{key}"), bytes))
        })
        .collect()
}

fn stored_json(path: PathBuf) -> Value {
    let bytes = fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_slice(&bytes).unwrap()
}

fn live_list(bot: &Bot) -> Value {
    stored_json(Path::new(bot.repository("bot")).join("uah-live/list.json"))
}

fn hub_index(bot: &Bot) -> Value {
    stored_json(Path::new(bot.repository("shared")).join("uah-live/index.json"))
}

fn entries(list: &Value) -> BTreeSet<String> {
    list["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["sha256"].as_str().unwrap().to_owned())
        .collect()
}

fn objects(bot: &Bot) -> BTreeMap<String, Vec<u8>> {
    files(&Path::new(bot.repository("bot")).join("uah-live/objects"))
}

fn tasks(bot: &Bot) -> BTreeMap<String, Vec<u8>> {
    files(&bot.path("owned").join("tasks"))
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// No plaintext path, content, or content hash may reach storage.
fn assert_opaque(bot: &Bot, contents: &[&[u8]]) {
    let mut needles: Vec<Vec<u8>> = ["tasks/", "a.md", "RULES"]
        .map(|name| name.as_bytes().to_vec())
        .into();
    for content in contents {
        needles.push(content.to_vec());
        needles.push(sha256_hex(content).into_bytes());
    }
    let stored = remote(bot);
    assert!(!stored.is_empty());
    for (key, bytes) in &stored {
        for needle in &needles {
            assert!(
                !bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_slice()),
                "{key} exposes {}",
                String::from_utf8_lossy(needle)
            );
        }
    }
}

// The primary defines tasks/ and pushes; the secondary pulls the same versions.
fn synced(fixture: &Fixture, secondary: &Bot, versions: &[(&str, &[u8])]) {
    for (path, bytes) in versions {
        write(fixture.alpha.path("owned").join(path), bytes);
    }
    define(fixture, &fixture.alpha, &["--task", "tasks/"]);
    let paths: Vec<_> = versions.iter().map(|(path, _)| *path).collect();
    assert_eq!(check(fixture, &fixture.alpha).paths("pushed"), paths);
    assert_eq!(check(fixture, secondary).paths("pulled"), paths);
    for (path, bytes) in versions {
        assert_eq!(
            fs::read(secondary.path("owned").join(path)).unwrap(),
            *bytes
        );
    }
}

fn conflict_copy(bot: &Bot) -> String {
    let copies: Vec<_> = tasks(bot)
        .into_keys()
        .filter(|name| {
            name.strip_prefix("a.conflict-")
                .and_then(|rest| rest.strip_suffix(".md"))
                .is_some_and(|sha| {
                    sha.len() == 8
                        && sha
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        })
        .collect();
    assert_eq!(copies.len(), 1, "one tasks/a.conflict-<8>.md: {copies:?}");
    copies.into_iter().next().unwrap()
}

#[test]
fn define_then_check_pushes_and_second_machine_pulls() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    write(primary.path("owned").join("tasks/a.md"), TASK);
    assert_eq!(
        define(
            &fixture,
            primary,
            &["--task", "tasks/", "--rule", "RULES.md"]
        ),
        1
    );
    let created = live_list(primary);
    assert_eq!(created["version"], 1);
    assert_eq!(created["entries"], json!([]), "the next check pushes roots");
    let salt = created["salt"].as_str().unwrap().to_owned();
    assert!(
        salt.len() == 32
            && salt
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{salt}"
    );
    let key = bot_key();
    assert_eq!(hub_index(primary)["bots"][key.as_str()], 1);

    let pushed = check(&fixture, primary);
    assert_eq!(pushed.paths("pushed"), ["RULES.md", "tasks/a.md"]);
    assert!(pushed.paths("pulled").is_empty());
    assert!(!pushed.conflicted());
    assert_eq!(pushed.report["hub_changed"], false);
    pushed.assert_interval(60);
    let list = live_list(primary);
    let revision = list["revision"].as_u64().unwrap();
    assert!(revision > 1, "{list}");
    let index = hub_index(primary);
    assert_eq!(index["bots"][key.as_str()], revision);
    let head = fixture.shared_head(primary);
    assert_eq!(
        index["hub"],
        json!({"revision": head["revision"], "snapshot": head["snapshot"]})
    );
    // Objects are keyed by their ciphertext SHA-256: nonce (24) + content + tag (16).
    let stored = objects(primary);
    assert_eq!(
        stored.keys().cloned().collect::<BTreeSet<_>>(),
        entries(&list)
    );
    for (name, bytes) in &stored {
        assert_eq!(&sha256_hex(bytes), name);
    }
    let mut sizes: Vec<_> = stored.values().map(Vec::len).collect();
    sizes.sort();
    let mut expected = vec![TASK.len() + 40, RULES.len() + 40];
    expected.sort();
    assert_eq!(sizes, expected);
    assert_opaque(primary, &[TASK, RULES]);

    let pulled = check(&fixture, &secondary);
    assert_eq!(pulled.paths("pulled"), ["RULES.md", "tasks/a.md"]);
    assert!(pulled.paths("pushed").is_empty());
    assert!(!pulled.conflicted());
    pulled.assert_interval(60);
    for (path, bytes) in [("tasks/a.md", TASK), ("RULES.md", RULES)] {
        let path = secondary.path("owned").join(path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(mode(&path), 0o600, "{}", path.display());
    }
    for bot in [primary, &secondary] {
        assert_eq!(mode(&bot.path("state").join("live.json")), 0o600);
    }
    let steady = check(&fixture, &secondary);
    assert!(steady.unchanged(), "{}", steady.report);
    assert_eq!(steady.report["hub_changed"], false);

    // Redefining drops entries outside the new roots and leaves local files alone.
    let redefined = define(&fixture, primary, &["--task", "tasks/"]);
    assert!(redefined > revision);
    let replaced = live_list(primary);
    assert_eq!(replaced["salt"], salt.as_str());
    let kept = entries(&replaced);
    assert_eq!(kept.len(), 1, "{replaced}");
    assert!(kept.is_subset(&entries(&list)));
    assert_eq!(stored[kept.first().unwrap()].len(), TASK.len() + 40);
    assert_eq!(hub_index(primary)["bots"][key.as_str()], redefined);
    assert_eq!(
        fs::read(primary.path("owned").join("RULES.md")).unwrap(),
        RULES
    );
    let after = check(&fixture, &secondary);
    assert!(!after.conflicted(), "{}", after.report);
    assert_eq!(
        fs::read(secondary.path("owned").join("tasks/a.md")).unwrap(),
        TASK
    );
}

#[test]
fn concurrent_edits_keep_both_versions() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(&fixture, &secondary, &[("tasks/a.md", TASK)]);
    let ours = b"primary edit\n";
    let theirs = b"secondary edit, a different version\n";
    write(primary.path("owned").join("tasks/a.md"), ours);
    write(secondary.path("owned").join("tasks/a.md"), theirs);

    let first = check(&fixture, primary);
    assert_eq!(first.paths("pushed"), ["tasks/a.md"]);
    assert!(!first.conflicted());
    let second = check(&fixture, &secondary);
    assert!(second.conflicted(), "{}", second.report);
    let copy = conflict_copy(&secondary);
    let expected = BTreeMap::from([
        ("a.md".to_owned(), ours.to_vec()),
        (copy.clone(), theirs.to_vec()),
    ]);
    assert_eq!(tasks(&secondary), expected);

    let third = check(&fixture, primary);
    assert_eq!(third.paths("pulled"), [format!("tasks/{copy}")]);
    assert_eq!(tasks(primary), expected);
}

#[test]
fn identical_concurrent_edits_do_not_conflict() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(&fixture, &secondary, &[("tasks/a.md", TASK)]);
    let same = b"the same edit on both machines\n";
    write(primary.path("owned").join("tasks/a.md"), same);
    write(secondary.path("owned").join("tasks/a.md"), same);

    assert_eq!(check(&fixture, primary).paths("pushed"), ["tasks/a.md"]);
    let second = check(&fixture, &secondary);
    assert!(!second.conflicted(), "{}", second.report);
    let third = check(&fixture, primary);
    assert!(third.unchanged(), "{}", third.report);
    let expected = BTreeMap::from([("a.md".to_owned(), same.to_vec())]);
    assert_eq!(tasks(primary), expected);
    assert_eq!(tasks(&secondary), expected);
    assert_eq!(entries(&live_list(primary)).len(), 1);
}

#[test]
fn deletion_propagates_and_remote_edit_beats_local_delete() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(
        &fixture,
        &secondary,
        &[("tasks/a.md", TASK), ("tasks/b.md", b"b before\n")],
    );
    let before = entries(&live_list(primary));

    fs::remove_file(primary.path("owned").join("tasks/a.md")).unwrap();
    check(&fixture, primary);
    let after = entries(&live_list(primary));
    assert_eq!(after.len(), 1);
    assert!(after.is_subset(&before));
    // Objects only the previous list referenced are deleted.
    assert_eq!(objects(primary).into_keys().collect::<BTreeSet<_>>(), after);
    check(&fixture, &secondary);
    assert_eq!(
        tasks(&secondary),
        BTreeMap::from([("b.md".to_owned(), b"b before\n".to_vec())])
    );

    fs::remove_file(secondary.path("owned").join("tasks/b.md")).unwrap();
    write(
        primary.path("owned").join("tasks/b.md"),
        b"b edited remotely\n",
    );
    assert_eq!(check(&fixture, primary).paths("pushed"), ["tasks/b.md"]);
    let edited = entries(&live_list(primary));
    assert_eq!(
        objects(primary).into_keys().collect::<BTreeSet<_>>(),
        edited
    );
    let restored = check(&fixture, &secondary);
    assert!(!restored.conflicted(), "{}", restored.report);
    let expected = BTreeMap::from([("b.md".to_owned(), b"b edited remotely\n".to_vec())]);
    assert_eq!(tasks(&secondary), expected);
    assert_eq!(entries(&live_list(primary)), edited);
    check(&fixture, primary);
    assert_eq!(tasks(primary), expected);
}

#[test]
fn unlisted_machine_is_rejected() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    write(primary.path("owned").join("tasks/a.md"), TASK);
    define(&fixture, primary, &["--task", "tasks/"]);
    check(&fixture, primary);
    let stored = remote(primary);

    let unlisted = machine(&fixture, "alpha-3", Some("machine-3"));
    let anonymous = machine(&fixture, "alpha-4", None);
    for bot in [&unlisted, &anonymous] {
        write(
            bot.path("owned").join("tasks/c.md"),
            b"must not be pushed\n",
        );
        let owned = files(&bot.path("owned"));
        let stderr = rejected(&fixture.cli(bot, &["live", "check"]));
        if bot.config["machine"]["id"].is_string() {
            assert!(stderr.contains(NOT_LISTED), "{stderr}");
        } else {
            assert!(stderr.to_lowercase().contains("machine"), "{stderr}");
        }
        assert_eq!(remote(primary), stored, "no remote live write");
        assert_eq!(files(&bot.path("owned")), owned, "no local live write");
    }
    assert_eq!(status(&fixture, &unlisted)["role"], Value::Null);
    // The listed machine passes the same membership check.
    assert_eq!(check(&fixture, &secondary).paths("pulled"), ["tasks/a.md"]);
}

#[test]
fn check_before_registration_passes_once_listed() {
    let fixture = primary();
    let primary = &fixture.alpha;
    write(primary.path("owned").join("tasks/a.md"), TASK);
    define(&fixture, primary, &["--task", "tasks/"]);
    check(&fixture, primary);
    let secondary = machine(&fixture, "alpha-2", Some("machine-2"));
    // The first check caches a hub whose registry does not list this machine yet.
    let stderr = rejected(&fixture.cli(&secondary, &["live", "check"]));
    assert!(stderr.contains(NOT_LISTED), "{stderr}");
    let stale = fixture.active_shared(&secondary);

    register(&fixture, &secondary);
    let listed = check(&fixture, &secondary);
    assert_eq!(listed.paths("pulled"), ["tasks/a.md"]);
    assert_eq!(listed.report["hub_changed"], true, "{}", listed.report);
    assert_ne!(fixture.active_shared(&secondary), stale);
    assert_eq!(status(&fixture, &secondary)["role"], "secondary");
}

#[test]
fn check_before_due_skips_without_network() {
    let fixture = primary();
    let bot = &fixture.alpha;
    write(bot.path("owned").join("tasks/a.md"), TASK);
    define(
        &fixture,
        bot,
        &["--task", "tasks/", "--base-seconds", "120"],
    );
    // The defined policy, not the default, schedules the next check.
    check(&fixture, bot).assert_interval(120);
    let stored = remote(bot);
    write(bot.path("owned").join("tasks/a.md"), b"edited before due\n");

    let repositories = ["shared", "bot"].map(|name| PathBuf::from(bot.repository(name)));
    let modes = repositories.each_ref().map(|path| mode(path));
    for path in &repositories {
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let output = fixture.cli(bot, &["live", "check"]);
    for (path, mode) in repositories.iter().zip(modes) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    success(&output);
    assert_eq!(json_line(&output), json!({"skipped": true}));
    assert_eq!(remote(bot), stored);
    assert_eq!(check(&fixture, bot).paths("pushed"), ["tasks/a.md"]);
}

#[test]
fn check_with_wrong_password_writes_nothing() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(&fixture, &secondary, &[("tasks/a.md", TASK)]);
    write(
        secondary.path("owned").join("tasks/b.md"),
        b"from the secondary\n",
    );
    assert_eq!(check(&fixture, &secondary).paths("pushed"), ["tasks/b.md"]);
    write(
        primary.path("owned").join("tasks/a.md"),
        b"local primary edit\n",
    );
    let owned = files(&primary.path("owned"));
    let stored = remote(primary);

    make_due(primary);
    let wrong = [("TEST_HUB_ALPHA_PASSWORD", WRONG_PASSWORD)];
    let output = fixture.cli_env(primary, &["live", "check"], &wrong);
    let stderr = rejected(&output);
    assert!(stderr.contains("decrypted"), "{stderr}");
    assert!(!text(&output).contains(WRONG_PASSWORD));
    assert_eq!(
        files(&primary.path("owned")),
        owned,
        "no local file written"
    );
    assert_eq!(remote(primary), stored, "nothing pushed with the wrong key");

    // A live failure stays out of State.failures and its next_action.
    let failed = status(&fixture, primary);
    assert!(!failed.to_string().contains(WRONG_PASSWORD));
    assert_eq!(failed["failure"], Value::Null, "{failed}");
    assert_ne!(failed["next_action"], "live");
    assert!(!failed["live"]["failure"].is_null(), "{failed}");
    // Backoff moves next_check forward, so the next minute's call is skipped.
    let retry = fixture.cli(primary, &["live", "check"]);
    success(&retry);
    assert_eq!(json_line(&retry), json!({"skipped": true}));

    let recovered = check(&fixture, primary);
    assert_eq!(recovered.paths("pulled"), ["tasks/b.md"]);
    assert_eq!(recovered.paths("pushed"), ["tasks/a.md"]);
    assert_eq!(status(&fixture, primary)["live"]["failure"], Value::Null);

    let stored = remote(primary);
    let output = fixture.cli_env(primary, &["live", "define", "--task", "tasks/"], &wrong);
    assert!(
        rejected(&output).contains(UNDECRYPTABLE),
        "{}",
        text(&output)
    );
    assert_eq!(remote(primary), stored);
}

#[test]
fn lower_list_revision_is_rejected() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(&fixture, &secondary, &[("tasks/a.md", TASK)]);
    let list_path = Path::new(primary.repository("bot")).join("uah-live/list.json");
    let older = fs::read(&list_path).unwrap();
    let older_revision = live_list(primary)["revision"].as_u64().unwrap();
    write(primary.path("owned").join("tasks/a.md"), b"newer version\n");
    assert_eq!(check(&fixture, primary).paths("pushed"), ["tasks/a.md"]);
    assert_eq!(check(&fixture, &secondary).paths("pulled"), ["tasks/a.md"]);
    let owned = files(&secondary.path("owned"));
    assert_eq!(owned["tasks/a.md"], b"newer version\n");

    // Replay the older list with a matching index hint; only the applied revision catches it.
    fs::write(&list_path, &older).unwrap();
    let index_path = Path::new(primary.repository("shared")).join("uah-live/index.json");
    let mut index = stored_json(index_path.clone());
    index["bots"][bot_key().as_str()] = json!(older_revision);
    fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
    let stored = remote(primary);

    make_due(&secondary);
    let stderr = rejected(&fixture.cli(&secondary, &["live", "check"]));
    assert!(stderr.contains(BACKWARDS), "{stderr}");
    assert_eq!(files(&secondary.path("owned")), owned);
    assert_eq!(remote(primary), stored);
}

#[test]
fn redefined_list_after_storage_reset_reaches_peers() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    synced(&fixture, &secondary, &[("tasks/a.md", TASK)]);
    let indexed = hub_index(primary)["bots"][bot_key().as_str()]
        .as_u64()
        .unwrap();

    // The documented recovery: delete uah-live/ from the Bot's storage, then define again.
    let storage = Path::new(primary.repository("bot")).join("uah-live");
    fs::remove_file(storage.join("list.json")).unwrap();
    fs::remove_dir_all(storage.join("objects")).unwrap();
    let redefined = define(&fixture, primary, &["--task", "tasks/"]);
    let added: &[u8] = b"written after the storage reset\n";
    write(primary.path("owned").join("tasks/b.md"), added);
    assert_eq!(
        check(&fixture, primary).paths("pushed"),
        ["tasks/a.md", "tasks/b.md"]
    );

    let pulled = check(&fixture, &secondary);
    assert_eq!(pulled.paths("pulled"), ["tasks/b.md"], "{}", pulled.report);
    assert!(!pulled.conflicted(), "{}", pulled.report);
    assert_eq!(
        tasks(&secondary),
        BTreeMap::from([
            ("a.md".to_owned(), TASK.to_vec()),
            ("b.md".to_owned(), added.to_vec()),
        ])
    );
    assert!(
        redefined > indexed,
        "a re-created list must start above the index entry {indexed}: revision {redefined}"
    );
}

#[test]
fn define_rejects_roots_outside_include_symlinked_or_excluded() {
    let mut fixture = primary();
    let owned = fixture.alpha.path("owned");
    for directory in ["other", "notes/real", "notes/secrets", "notes/drafts"] {
        fs::create_dir_all(owned.join(directory)).unwrap();
    }
    write(owned.join("notes/real/x.md"), b"x\n");
    symlink(owned.join("notes/real"), owned.join("notes/linked")).unwrap();
    fixture.alpha.config["data"]["exclude"] = json!(["drafts"]);
    fixture.alpha.save();
    let list = Path::new(fixture.alpha.repository("bot")).join("uah-live/list.json");
    let cases: [&[&str]; 9] = [
        &["--task", "other/"],
        &["--task", "notes/linked/"],
        &["--rule", "notes/linked/x.md"],
        &["--task", "notes/secrets/"],
        &["--task", "notes/drafts/"],
        &["--task", "notes/missing/"],
        &["--task", "../escape/"],
        &[],
        &["--task", "tasks/", "--base-seconds", "59"],
    ];
    for case in cases {
        let args: Vec<_> = ["live", "define"]
            .into_iter()
            .chain(case.iter().copied())
            .collect();
        rejected(&fixture.cli(&fixture.alpha, &args));
        assert!(!list.exists(), "{case:?} must not create a live list");
    }
    assert_eq!(
        define(
            &fixture,
            &fixture.alpha,
            &["--task", "tasks/", "--rule", "RULES.md"]
        ),
        1
    );
    // `.` covers the whole owned tree.
    fixture.alpha.config["data"]["include"] = json!(["."]);
    fixture.alpha.save();
    assert!(define(&fixture, &fixture.alpha, &["--task", "other/"]) > 1);
}

#[test]
fn status_reports_machine_role_and_live() {
    let (fixture, secondary) = machines();
    let primary = &fixture.alpha;
    let initial = status(&fixture, primary);
    assert_eq!(initial["machine"], machine_hash("machine-1"));
    assert_eq!(initial["role"], "primary");
    assert_eq!(initial["live"], Value::Null);
    assert_ne!(initial["next_action"], "live");
    let fresh = status(&fixture, &secondary);
    assert_eq!(fresh["machine"], machine_hash("machine-2"));
    assert_eq!(fresh["role"], Value::Null, "no cached registry yet");
    assert_eq!(fresh["live"], Value::Null);

    write(primary.path("owned").join("tasks/a.md"), TASK);
    define(&fixture, primary, &["--task", "tasks/"]);
    let pushed = check(&fixture, primary);
    let checked = status(&fixture, primary);
    assert_eq!(checked["role"], "primary");
    assert_eq!(
        checked["live"],
        json!({"next_check": pushed.report["next_check"], "failure": null})
    );

    let pulled = check(&fixture, &secondary);
    let listed = status(&fixture, &secondary);
    assert_eq!(listed["role"], "secondary");
    assert_eq!(listed["next_action"], "live");
    assert_eq!(listed["remediation"], json!([SECONDARY]));
    assert_eq!(
        listed["live"],
        json!({"next_check": pulled.report["next_check"], "failure": null})
    );

    let anonymous = status(&fixture, &machine(&fixture, "alpha-3", None));
    assert_eq!(anonymous["machine"], Value::Null);
    assert_eq!(anonymous["role"], Value::Null);

    // A cached registry that cannot be read yields no role, not a status error.
    let cached = Path::new(listed["active_shared"].as_str().unwrap()).join("registry.yaml");
    fs::set_permissions(&cached, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&cached, b"{").unwrap();
    assert_eq!(status(&fixture, &secondary)["role"], Value::Null);
}

#[test]
fn check_while_another_operation_runs_reports_busy() {
    let fixture = primary();
    let bot = &fixture.alpha;
    write(bot.path("owned").join("tasks/a.md"), TASK);
    define(&fixture, bot, &["--task", "tasks/"]);
    make_due(bot);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bot.path("state").join("operation.lock"))
        .unwrap();
    lock.lock().unwrap();
    let output = fixture.cli(bot, &["live", "check"]);
    drop(lock);
    success(&output);
    assert_eq!(
        json_line(&output),
        json!({"skipped": true, "reason": "busy"})
    );
    assert!(objects(bot).is_empty());
    assert_eq!(check(&fixture, bot).paths("pushed"), ["tasks/a.md"]);
}

// A live check holds the Bot state lock; a scheduled sync then must not count as a failure.
#[test]
fn scheduled_sync_skips_while_bot_state_is_busy() {
    let fixture = primary();
    let bot = &fixture.alpha;
    let file = bot.path("state").join("status.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    state["next_due"] = json!(Utc::now().timestamp() - 1);
    fs::write(&file, state.to_string()).unwrap();
    let bytes = fs::read(&file).unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(bot.path("state").join("operation.lock"))
        .unwrap();
    lock.lock().unwrap();
    let output = fixture.cli(bot, &["sync", "--scheduled"]);
    drop(lock);
    success(&output);
    assert_eq!(
        json_line(&output),
        json!({"skipped": true, "reason": "busy"})
    );
    assert_eq!(fs::read(&file).unwrap(), bytes);
}

#[test]
fn unreadable_live_state_names_its_path() {
    let fixture = primary();
    let bot = &fixture.alpha;
    define(&fixture, bot, &["--task", "tasks/"]);
    check(&fixture, bot);
    let path = bot.path("state").join("live.json");
    for bytes in [&b"{"[..], br#"{"unexpected":true}"#] {
        fs::write(&path, bytes).unwrap();
        let stderr = rejected(&fixture.cli(bot, &["live", "check"]));
        assert!(stderr.contains(&path.display().to_string()), "{stderr}");
    }
}

#[test]
fn check_without_live_list_changes_nothing() {
    let fixture = primary();
    let bot = &fixture.alpha;
    write(bot.path("owned").join("tasks/a.md"), TASK);
    let checked = check(&fixture, bot);
    assert!(checked.unchanged(), "{}", checked.report);
    assert!(
        !Path::new(bot.repository("bot"))
            .join("uah-live/list.json")
            .exists()
    );
    assert!(objects(bot).is_empty());
}

#[test]
fn check_skips_symlinks_and_excluded_files_in_roots() {
    let mut fixture = primary();
    fixture.alpha.config["data"]["exclude"] = json!(["*.tmp"]);
    fixture.alpha.save();
    let bot = &fixture.alpha;
    let tasks = bot.path("owned").join("tasks");
    write(tasks.join("a.md"), TASK);
    let hidden: [(&str, &[u8]); 3] = [
        (".env", b"API_TOKEN=live-secret-6e1f\n"),
        ("debug.log", b"log line live-secret-7a2b\n"),
        ("scratch.tmp", b"draft live-secret-8c3d\n"),
    ];
    for (name, bytes) in hidden {
        write(tasks.join(name), bytes);
    }
    symlink(bot.path("owned").join("RULES.md"), tasks.join("linked.md")).unwrap();
    define(&fixture, bot, &["--task", "tasks/"]);
    assert_eq!(check(&fixture, bot).paths("pushed"), ["tasks/a.md"]);
    assert_eq!(entries(&live_list(bot)).len(), 1);
    assert_opaque(bot, &[TASK, RULES, hidden[0].1, hidden[1].1, hidden[2].1]);
}

// Target alpha and requester beta are separate Bots of one hub.
fn hub(shared: bool) -> (Fixture, Bot) {
    let mut fixture = Fixture::new();
    configure(&mut fixture.alpha, Some("machine-1"));
    write(fixture.alpha.path("owned").join("RULES.md"), RULES);
    fixture.alpha.config["sharing"]["bot_data"] = json!(shared);
    fixture.alpha.save();
    let beta = fixture.bot("beta", "beta-1");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    fixture.ok(&fixture.alpha, &["setup"]);
    fixture.ok(&beta, &["setup"]);
    (fixture, beta)
}

fn request_name(bytes: &[u8]) -> String {
    format!("tasks/beta-{}.md", &sha256_hex(bytes)[..8])
}

// Beta files the request from outside both owned trees.
fn request(fixture: &Fixture, beta: &Bot, file: &str, bytes: &[u8]) -> Output {
    let path = beta.file.parent().unwrap().join("requests").join(file);
    write(&path, bytes);
    fixture.cli(
        beta,
        &["live", "request", "alpha", "--file", path.to_str().unwrap()],
    )
}

fn hub_mark(head: &Value) -> Value {
    json!({"revision": head["revision"], "snapshot": head["snapshot"]})
}

// Alpha publishes a shared edit from its accepted authoring base.
fn publish_knowledge(fixture: &Fixture) {
    let alpha = &fixture.alpha;
    fixture.author_current(alpha);
    write(alpha.path("shared").join("knowledge.md"), KNOWLEDGE);
    fixture.ok(alpha, &["registry", "publish"]);
}

// Alpha on the fake R2: curl and restic both resolve to the local repositories.
fn r2() -> (Fixture, Bot, PathBuf) {
    let mut fixture = Fixture::new();
    configure(&mut fixture.alpha, Some("machine-1"));
    write(fixture.alpha.path("owned").join("RULES.md"), RULES);
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let local = Bot {
        file: fixture.alpha.file.clone(),
        config: fixture.alpha.config.clone(),
    };
    for name in ["shared", "bot"] {
        fs::write(tool.join(format!("repo-{name}")), local.repository(name)).unwrap();
    }
    let attestation = fixture.temp.path().join("attestation.json");
    write(
        &attestation,
        json!({
            "expires_at": "2999-01-01T00:00:00Z",
            "repositories": [R2_SHARED, R2_BOT],
            "max_source_bytes": 1_000_000,
        })
        .to_string(),
    );
    fs::set_permissions(&attestation, fs::Permissions::from_mode(0o600)).unwrap();
    let alpha = &mut fixture.alpha;
    alpha.config["storage"]["provider"] = json!("r2");
    alpha.config["repositories"]["shared"]["url"] = json!(R2_SHARED);
    alpha.config["repositories"]["bot"]["url"] = json!(R2_BOT);
    alpha.config["budget"]["attestation_file"] = json!(attestation);
    alpha.save();
    // The fixture's helpers pass no credentials, so they come from the config-adjacent .env.
    dotenv(
        alpha,
        format!("AWS_ACCESS_KEY_ID=synthetic-access-key\nAWS_SECRET_ACCESS_KEY={BOT_PASSWORD}\n"),
    );
    fixture.registry(&fixture.alpha, &[&fixture.alpha]);
    fixture.fake_r2(
        &fixture.alpha,
        &[
            ("shared", local.repository("shared")),
            ("bot", local.repository("bot")),
        ],
    );
    fixture.ok(&fixture.alpha, &["setup"]);
    (fixture, local, tool)
}

#[test]
fn request_reaches_shared_target() {
    let (fixture, beta) = hub(true);
    let alpha = &fixture.alpha;
    write(alpha.path("owned").join("tasks/a.md"), TASK);
    define(&fixture, alpha, &["--task", "tasks/"]);
    assert_eq!(check(&fixture, alpha).paths("pushed"), ["tasks/a.md"]);
    let before = live_list(alpha);

    let name = request_name(REQUEST);
    let output = request(&fixture, &beta, "request.md", REQUEST);
    success(&output);
    assert_eq!(json_line(&output), json!({"requested": name}));
    let after = live_list(alpha);
    let revision = after["revision"].as_u64().unwrap();
    assert!(revision > before["revision"].as_u64().unwrap(), "{after}");
    assert_eq!(hub_index(alpha)["bots"][bot_key().as_str()], revision);
    // Existing entries stay byte-identical; the request only adds one.
    let old = before["entries"].as_array().unwrap();
    let new = after["entries"].as_array().unwrap();
    assert!(old.iter().all(|entry| new.contains(entry)), "{after}");
    assert_eq!(new.len(), old.len() + 1, "{after}");
    let added: Vec<_> = entries(&after)
        .difference(&entries(&before))
        .cloned()
        .collect();
    assert_eq!(objects(alpha)[&added[0]].len(), REQUEST.len() + 40);
    assert_opaque(alpha, &[TASK, REQUEST, name.as_bytes()]);

    let pulled = check(&fixture, alpha);
    assert_eq!(pulled.paths("pulled"), [name.as_str()]);
    assert!(pulled.paths("pushed").is_empty(), "{}", pulled.report);
    assert!(!pulled.conflicted(), "{}", pulled.report);
    let path = alpha.path("owned").join(&name);
    assert_eq!(fs::read(&path).unwrap(), REQUEST);
    assert_eq!(mode(&path), 0o600);
    assert_eq!(
        fs::read(alpha.path("owned").join("tasks/a.md")).unwrap(),
        TASK
    );
}

#[test]
fn request_to_private_target_is_rejected() {
    let (fixture, beta) = hub(false);
    let alpha = &fixture.alpha;
    let cached = fixture.active_shared(&beta).join("registry.yaml");
    let registry: Value = serde_saphyr::from_slice(&fs::read(cached).unwrap()).unwrap();
    let entry = registry["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "alpha")
        .unwrap();
    assert_eq!(entry["shared"], false, "beta's cached registry: {registry}");
    // The target accepts requests in every other respect.
    define(&fixture, alpha, &["--task", "tasks/"]);
    let stored = remote(alpha);

    let stderr = rejected(&request(&fixture, &beta, "request.md", REQUEST));
    assert!(stderr.contains(PRIVATE_TARGET), "{stderr}");
    assert_eq!(remote(alpha), stored, "no live write for a private target");
}

#[test]
fn request_needs_a_task_directory() {
    let (fixture, beta) = hub(true);
    let alpha = &fixture.alpha;
    write(alpha.path("owned").join("tasks/a.md"), TASK);
    // A task file root and a rule directory root: neither takes requests.
    define(
        &fixture,
        alpha,
        &["--task", "tasks/a.md", "--rule", "notes/"],
    );
    let stored = remote(alpha);
    let stderr = rejected(&request(&fixture, &beta, "request.md", REQUEST));
    assert!(stderr.contains(NO_TASK_DIRECTORY), "{stderr}");
    assert_eq!(remote(alpha), stored);

    define(&fixture, alpha, &["--task", "tasks/"]);
    let output = request(&fixture, &beta, "request.md", REQUEST);
    success(&output);
    assert_eq!(
        json_line(&output),
        json!({"requested": request_name(REQUEST)})
    );
}

#[test]
fn request_rejects_oversized_or_non_regular_files() {
    let (fixture, beta) = hub(true);
    let alpha = &fixture.alpha;
    define(&fixture, alpha, &["--task", "tasks/"]);
    let stored = remote(alpha);
    let requests = beta.file.parent().unwrap().join("requests");
    let limit = vec![b'r'; REQUEST_LIMIT];
    write(requests.join("oversized.md"), [&limit[..], b"r"].concat());
    write(requests.join("small.md"), REQUEST);
    symlink(requests.join("small.md"), requests.join("linked.md")).unwrap();
    fs::create_dir_all(requests.join("folder.md")).unwrap();
    for file in ["oversized.md", "linked.md", "folder.md"] {
        let path = requests.join(file);
        rejected(&fixture.cli(
            &beta,
            &["live", "request", "alpha", "--file", path.to_str().unwrap()],
        ));
        assert_eq!(remote(alpha), stored, "{file} must not be requested");
    }

    let name = request_name(&limit);
    let output = request(&fixture, &beta, "limit.md", &limit);
    success(&output);
    assert_eq!(json_line(&output), json!({"requested": name}));
    assert_eq!(check(&fixture, alpha).paths("pulled"), [name.as_str()]);
    assert_eq!(fs::read(alpha.path("owned").join(&name)).unwrap(), limit);
}

#[test]
fn hub_publish_updates_index_and_peers_import() {
    let (fixture, beta) = hub(true);
    let alpha = &fixture.alpha;
    // Setup's head CAS also marks the hub in the index.
    let mut index = json!({
        "version": 1,
        "hub": hub_mark(&fixture.shared_head(alpha)),
        "bots": {},
    });
    assert_eq!(hub_index(alpha), index);
    define(&fixture, alpha, &["--task", "tasks/"]);
    index["bots"][bot_key().as_str()] = json!(1);
    assert_eq!(hub_index(alpha), index);
    let before = fixture.active_shared(&beta);
    let current = check(&fixture, &beta);
    assert_eq!(current.report["hub_changed"], false, "{}", current.report);

    fixture.ok(alpha, &["sync"]);
    publish_knowledge(&fixture);
    let head = fixture.shared_head(alpha);
    index["hub"] = hub_mark(&head);
    assert_eq!(hub_index(alpha), index, "publish marks the new head");

    let changed = check(&fixture, &beta);
    assert_eq!(changed.report["hub_changed"], true, "{}", changed.report);
    let after = fixture.active_shared(&beta);
    assert_ne!(after, before);
    assert_eq!(
        head["snapshot"],
        after.file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(fs::read(after.join("knowledge.md")).unwrap(), KNOWLEDGE);
    let imported = check(&fixture, &beta);
    assert_eq!(imported.report["hub_changed"], false, "{}", imported.report);
}

#[test]
fn index_repair_follows_a_newer_cache() {
    let fixture = primary();
    let alpha = &fixture.alpha;
    define(&fixture, alpha, &["--task", "tasks/"]);
    let stale = hub_mark(&fixture.shared_head(alpha));
    publish_knowledge(&fixture);
    let current = hub_mark(&fixture.shared_head(alpha));
    assert_ne!(stale, current);
    let path = Path::new(alpha.repository("shared")).join("uah-live/index.json");
    let mut index = stored_json(path.clone());
    assert_eq!(index["hub"], current);
    // As if an older CLI had published: the index keeps the previous head.
    index["hub"] = stale;
    fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();

    // Alpha's cache already holds the newer head, so the check imports nothing.
    let repaired = check(&fixture, alpha);
    assert_eq!(repaired.report["hub_changed"], false, "{}", repaired.report);
    index["hub"] = current;
    assert_eq!(hub_index(alpha), index);
}

#[test]
fn failed_hub_mark_keeps_publish_successful_and_is_retried() {
    let fixture = primary();
    let alpha = &fixture.alpha;
    define(&fixture, alpha, &["--task", "tasks/"]);
    let before = fixture.shared_head(alpha);
    let path = Path::new(alpha.repository("shared")).join("uah-live/index.json");
    let index = fs::read(&path).unwrap();
    // The local store reads and replaces only regular files, so the hub mark fails.
    let aside = fixture.temp.path().join("index-aside.json");
    fs::rename(&path, &aside).unwrap();
    fs::create_dir(&path).unwrap();
    publish_knowledge(&fixture);
    let head = fixture.shared_head(alpha);
    assert!(head["revision"].as_u64() > before["revision"].as_u64());
    // Still an empty directory, so the old index returns unchanged.
    fs::remove_dir(&path).unwrap();
    fs::rename(&aside, &path).unwrap();

    check(&fixture, alpha);
    let mut expected: Value = serde_json::from_slice(&index).unwrap();
    expected["hub"] = hub_mark(&head);
    assert_eq!(hub_index(alpha), expected);
}

#[test]
fn steady_state_check_reads_only_the_index() {
    let (fixture, local, tool) = r2();
    let bot = &fixture.alpha;
    write(bot.path("owned").join("tasks/a.md"), TASK);
    assert_eq!(
        live(&fixture, bot, &["define", "--task", "tasks/"]),
        json!({"defined": true, "revision": 1})
    );
    assert_eq!(check(&fixture, bot).paths("pushed"), ["tasks/a.md"]);
    let list = live_list(&local);
    assert_eq!(
        objects(&local).into_keys().collect::<BTreeSet<_>>(),
        entries(&list)
    );
    assert_eq!(
        hub_index(&local)["bots"][bot_key().as_str()],
        list["revision"]
    );
    assert_opaque(&local, &[TASK]);
    // The push rewrote the index; this check absorbs that one change.
    let settled = check(&fixture, bot);
    assert!(settled.unchanged(), "{}", settled.report);
    let log = fixture.temp.path().join("head-curl/requests.jsonl");
    let traffic = fs::read_to_string(&log).unwrap();
    for needle in ["tasks", "a.md", &sha256_hex(TASK)] {
        assert!(!traffic.contains(needle), "a request URL exposes {needle}");
    }

    fs::write(&log, "").unwrap();
    fs::write(tool.join("calls"), "").unwrap();
    let steady = check(&fixture, bot);
    assert!(steady.unchanged(), "{}", steady.report);
    assert_eq!(steady.report["hub_changed"], false);
    let index =
        fs::read(Path::new(local.repository("shared")).join("uah-live/index.json")).unwrap();
    let requests: Vec<Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        requests,
        [json!({
            "method": "GET",
            "url": format!("{}/uah-live/index.json", R2_SHARED.strip_prefix("s3:").unwrap()),
            "status": 304,
            "conditions": {"if-none-match": [format!("\"{}\"", sha256_hex(&index))]},
        })]
    );
    // Restic reaches storage too; only its local version probe may run.
    let calls = fs::read_to_string(tool.join("calls")).unwrap();
    assert!(calls.lines().all(|call| call == "version"), "{calls}");
}

#[test]
fn restic_check_passes_after_live_objects() {
    let fixture = primary();
    let alpha = &fixture.alpha;
    write(alpha.path("owned").join("tasks/a.md"), TASK);
    write(alpha.path("owned").join("tasks/b.md"), b"b before\n");
    define(&fixture, alpha, &["--task", "tasks/", "--rule", "RULES.md"]);
    assert_eq!(
        check(&fixture, alpha).paths("pushed"),
        ["RULES.md", "tasks/a.md", "tasks/b.md"]
    );
    // A removal also deletes the object only the previous list referenced.
    fs::remove_file(alpha.path("owned").join("tasks/a.md")).unwrap();
    check(&fixture, alpha);
    assert_eq!(objects(alpha).len(), 2);
    assert!(
        Path::new(alpha.repository("shared"))
            .join("uah-live/index.json")
            .is_file()
    );
    for repository in ["shared", "bot"] {
        let output = fixture.restic(alpha, repository, &["check"]);
        assert!(output.status.success(), "{repository}: {}", text(&output));
    }
}
