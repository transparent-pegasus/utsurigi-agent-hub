//! `live check` and `live define`: listed files merged three ways through the Bot's CAS list.

use crate::config::{Config, Repository, no_symlinks};
use crate::error::{Error, IoContext, Result};
use crate::live::cadence::{backoff, interval, jitter};
use crate::live::crypto::{self, Key, Kind, SALT_BYTES};
use crate::live::format::{
    Class, Entry, HubMark, Index, List, MAX_BYTES, MAX_ENTRIES, Root, Spec, bot_key, hex,
    relative_path, sha256_hex, unhex,
};
use crate::live::merge::{self, Action, Base};
use crate::live::policy::Policy;
use crate::live::state::{Applied, Live, Seen};
use crate::objects::{Condition, Fetched, MAX_OBJECT_BYTES, ObjectStore, Written};
use crate::operations::{hub_mark, import_hub, status_time};
use crate::registry::Registry;
use crate::restic::password;
use crate::selection::{excluded, exclusions, open_source};
use crate::state::Lock;
use crate::{budget, shared};
use chacha20poly1305::aead::Generate;
use chrono::Utc;
use globset::GlobSet;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const INDEX: &str = "uah-live/index.json";
const LIST: &str = "uah-live/list.json";
const CAS_ATTEMPTS: usize = 16;
// Sealing adds a 24-byte nonce and a 16-byte tag; the object must fit MAX_OBJECT_BYTES.
const MAX_PLAINTEXT: u64 = MAX_BYTES - 40;
// Writes staged inside live roots carry this prefix, so a scan never lists them.
const STAGING: &str = ".uah-live-";

/// The index from one read: `None` when unchanged since the token, `Some(None)` when absent.
type Observed = Option<Option<(Index, String)>>;

#[derive(Default)]
struct Report {
    hub_changed: bool,
    pulled: BTreeSet<String>,
    pushed: BTreeSet<String>,
    conflicts: BTreeSet<String>,
}

/// A merge that reached the list, still to be applied locally.
struct Merged {
    wrote: bool,
    revision: u64,
    salt: String,
    spec: Spec,
    base: BTreeMap<String, Base>,
    applies: Vec<Apply>,
    obsolete: Vec<String>,
    pushed: BTreeSet<String>,
    conflicts: BTreeSet<String>,
}

/// A local replacement, or a removal when `content` is `None`. It applies only while the file
/// still has `expected`, the plaintext SHA-256 the merge saw.
struct Apply {
    path: String,
    expected: Option<String>,
    content: Option<Vec<u8>>,
    pulled: bool,
}

enum Disk {
    Absent,
    File(Vec<u8>),
    Other,
}

impl Disk {
    fn sha(&self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::File(plain) => Some(sha256_hex(plain)),
            // Matches no plaintext SHA-256, so a directory or symlink is never overwritten.
            Self::Other => Some(String::new()),
        }
    }
}

/// The paths a spec tracks: inside a root and not excluded.
struct Scope<'a> {
    roots: &'a [Root],
    globs: &'a GlobSet,
}

impl Scope<'_> {
    fn tracks(&self, path: &str) -> bool {
        self.roots.iter().any(|root| covers(&root.path, path))
            && !excluded(Path::new(path), self.globs)
    }

    /// Plaintext SHA-256 of every tracked regular file, by owned-relative path.
    fn scan(&self, owned: &Path) -> Result<BTreeMap<String, String>> {
        let mut files = BTreeMap::new();
        for root in self.roots {
            match root.path.strip_suffix('/') {
                Some(directory) => self.walk(owned, directory, &mut files)?,
                None => match disk(owned, &root.path)? {
                    Disk::File(plain) if self.tracks(&root.path) => {
                        files.insert(root.path.clone(), sha256_hex(&plain));
                    }
                    Disk::Other => {
                        return Err(Error::new(format!(
                            "Live root must be a regular file: {}",
                            root.path
                        )));
                    }
                    _ => (),
                },
            }
        }
        Ok(files)
    }

    fn walk(
        &self,
        owned: &Path,
        directory: &str,
        files: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        let path = owned.join(directory);
        no_symlinks(&path)?;
        let entries = match fs::read_dir(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            entries => entries.at("Cannot read live directory", &path)?,
        };
        for entry in entries {
            let entry = entry.at("Cannot read live directory", &path)?;
            // A name the list cannot carry is not synced.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = format!("{directory}/{name}");
            if name.starts_with(STAGING)
                || relative_path(&relative).is_err()
                || !self.tracks(&relative)
            {
                continue;
            }
            let kind = entry
                .file_type()
                .at("Cannot inspect live file", &entry.path())?;
            if kind.is_dir() {
                self.walk(owned, &relative, files)?;
            } else if kind.is_file()
                && let Disk::File(plain) = disk(owned, &relative)?
            {
                files.insert(relative, sha256_hex(&plain));
            }
        }
        Ok(())
    }
}

pub fn check(config: &Config) -> Result<()> {
    config.runtime()?;
    let root = state_root(config);
    let Some(_lock) = Lock::try_acquire(root)? else {
        println!("{}", json!({"skipped": true, "reason": "busy"}));
        return Ok(());
    };
    let mut live = Live::load(root)?.unwrap_or_default();
    let now = Utc::now().timestamp();
    if now < live.next_check {
        println!("{}", json!({"skipped": true}));
        return Ok(());
    }
    let result = run(config, root, &mut live, now);
    let (policy, has_tasks) = live
        .list
        .as_ref()
        .map_or((Policy::default(), false), |list| {
            let tasks = list.spec.roots.iter().any(|root| root.class == Class::Task);
            (list.spec.policy, tasks)
        });
    let seconds = match &result {
        Ok(_) => {
            live.failures = 0;
            live.failure = None;
            interval(&policy, has_tasks, now, live.changed_at)
        }
        Err(error) => {
            live.failures = live.failures.saturating_add(1);
            live.failure = Some(sanitized(&error.message));
            // The retry reads the whole index, so nothing seen before the failure is skipped.
            live.index = None;
            backoff(&policy, has_tasks, live.failures)
        }
    };
    live.next_check = now.saturating_add(jitter(seconds, unit()) as i64);
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            // Preserve the check's own error if recording it fails as well.
            let _ = live.save(root);
            return Err(error);
        }
    };
    live.save(root)?;
    println!(
        "{}",
        json!({
            "checked": true, "hub_changed": report.hub_changed, "pulled": report.pulled,
            "pushed": report.pushed, "conflicts": report.conflicts,
            "next_check": status_time(Some(live.next_check))?,
        })
    );
    Ok(())
}

fn run(config: &Config, root: &Path, live: &mut Live, now: i64) -> Result<Report> {
    let mut cached = joined(config)?;
    let shared = ObjectStore::new(config, url(&config.repositories.shared))?;
    // A pending write, or a cache newer than the index last seen, needs the index body.
    let whole = live.pending_index.is_some()
        || live.pending_hub.is_some()
        || live
            .index
            .as_ref()
            .is_some_and(|seen| cached.revision > seen.hub);
    let token = live
        .index
        .as_ref()
        .filter(|_| !whole)
        .map(|seen| seen.token.clone());
    let observed = read_index(&shared, token.as_deref())?;
    let before = live.list.as_ref().map(|list| list.revision);
    let mut report = Report::default();
    let mut repair = false;
    let mut remote_changed = false;
    match &observed {
        Some(Some((index, token))) => {
            if index.hub.revision > cached.revision {
                import_hub(config)?;
                let imported = hub_mark(config)?.ok_or_else(missing_cache)?;
                report.hub_changed = imported != cached;
                cached = imported;
            }
            if index.hub.revision == cached.revision && index.hub.snapshot != cached.snapshot {
                return Err(Error::new(
                    "Live index and shared cache name different snapshots for one hub revision; inspect the shared head",
                ));
            }
            repair = cached.revision > index.hub.revision;
            let listed = index.bots.get(&bot_key(&config.hub, config.id()));
            remote_changed = listed.copied().unwrap_or(0) != before.unwrap_or(0);
            live.index = Some(Seen {
                token: token.clone(),
                hub: index.hub.revision,
            });
        }
        Some(None) => live.index = None,
        None => (),
    }
    let owned = owned(config);
    let globs = exclusions(config)?;
    let local_changed = match &live.list {
        Some(list) => {
            let scope = Scope {
                roots: &list.spec.roots,
                globs: &globs,
            };
            let base: BTreeMap<String, String> = live
                .base
                .iter()
                .filter(|(path, _)| scope.tracks(path))
                .map(|(path, base)| (path.clone(), base.plain.clone()))
                .collect();
            scope.scan(owned)? != base
        }
        None => false,
    };
    let merged = if remote_changed || local_changed {
        merge_list(config, owned, &globs, live)?
    } else {
        None
    };
    if let Some(merged) = merged.as_ref().filter(|merged| merged.wrote) {
        // Recorded first, so a crash before the index write still retries it.
        live.pending_index = Some(merged.revision);
        live.save(root)?;
    }
    if live.pending_index.is_some() || live.pending_hub.is_some() || repair {
        update_index(config, &shared, live, observed, &cached)?;
    }
    if let Some(merged) = merged {
        apply(config, owned, live, merged, &mut report)?;
    }
    if report.hub_changed || live.list.as_ref().map(|list| list.revision) != before {
        live.changed_at = Some(now);
    }
    Ok(report)
}

/// Imports the hub on first use, then requires this machine to belong to the Bot.
fn joined(config: &Config) -> Result<HubMark> {
    if hub_mark(config)?.is_none() {
        import_hub(config)?;
    }
    let active = shared::active(config)?.ok_or_else(missing_cache)?;
    if Registry::load(&active, config)?.role(config).is_none() {
        return Err(Error::new(
            "This machine is not listed for the Bot; add status.machine to its registry machines",
        ));
    }
    hub_mark(config)?.ok_or_else(missing_cache)
}

/// Reads the list and merges it with local files; `None` when the Bot defines no list.
fn merge_list(
    config: &Config,
    owned: &Path,
    globs: &GlobSet,
    live: &mut Live,
) -> Result<Option<Merged>> {
    let store = ObjectStore::new(config, url(&config.repositories.bot))?;
    let password = password(&config.repositories.bot)?;
    'attempt: for _ in 0..CAS_ATTEMPTS {
        let Some((list, token)) = read_list(&store)? else {
            live.list = None;
            live.base.clear();
            return Ok(None);
        };
        // The spec decrypts only under the right key, so nothing is ever sealed with a wrong one.
        let key = derive(&password, &list)?;
        let spec = open_spec(&key, config, &list)?;
        match &live.list {
            Some(applied) if applied.salt == list.salt => {
                if list.revision < applied.revision {
                    return Err(Error::new(
                        "Live list revision went backwards; local files were left unchanged",
                    ));
                }
            }
            // A new salt means the list was defined afresh: nothing applied before is a base.
            _ => live.base.clear(),
        }
        let scope = Scope {
            roots: &spec.roots,
            globs,
        };
        live.base.retain(|path, _| scope.tracks(path));
        let names = open_names(&key, config, &list)?;
        let listed: BTreeMap<String, String> = names
            .iter()
            .zip(&list.entries)
            .filter(|(path, _)| scope.tracks(path))
            .map(|(path, entry)| (path.clone(), entry.sha256.clone()))
            .collect();
        let local = scope.scan(owned)?;
        let actions = merge::plan(&live.base, &listed, &local);
        let mut fetched = BTreeMap::new();
        for action in &actions {
            if let Action::Pull { object, .. }
            | Action::Conflict {
                remote: Some(object),
                ..
            } = action
            {
                match download(&store, &key, config, object)? {
                    Some(plain) => {
                        fetched.insert(object.clone(), plain);
                    }
                    // A concurrent writer replaced and deleted it: read the list again.
                    None => continue 'attempt,
                }
            }
        }
        let mut base = live.base.clone();
        let mut pushes = Vec::new();
        let mut dropped = BTreeSet::new();
        let mut applies = Vec::new();
        let mut conflicts = BTreeSet::new();
        for action in actions {
            match action {
                Action::Push(path) => {
                    // A file gone since the scan is merged by the next check.
                    if let Disk::File(plain) = disk(owned, &path)? {
                        pushes.push((path, plain));
                    }
                }
                Action::Delete(path) => {
                    base.remove(&path);
                    dropped.insert(path);
                }
                Action::Pull { path, object } => {
                    let content = fetched.remove(&object).expect("downloaded above");
                    base.insert(path.clone(), base_of(object, &content));
                    applies.push(Apply {
                        expected: local.get(&path).cloned(),
                        path,
                        content: Some(content),
                        pulled: true,
                    });
                }
                Action::Remove(path) => {
                    base.remove(&path);
                    applies.push(Apply {
                        expected: local.get(&path).cloned(),
                        path,
                        content: None,
                        pulled: true,
                    });
                }
                Action::Forget(path) => {
                    base.remove(&path);
                }
                Action::Conflict { path, remote } => {
                    let Disk::File(mine) = disk(owned, &path)? else {
                        continue;
                    };
                    let sha = sha256_hex(&mine);
                    let theirs = remote.map(|object| {
                        let content = fetched.remove(&object).expect("downloaded above");
                        (object, content)
                    });
                    if let Some((object, content)) = &theirs
                        && sha256_hex(content) == sha
                    {
                        base.insert(path, base_of(object.clone(), content));
                        continue;
                    }
                    // The local version is saved under its own name before the path changes.
                    let (copy, fresh) = conflict_copy(owned, &path, &sha, &listed, &base)?;
                    if fresh {
                        write_file(owned, &copy, &mine, false)?;
                        if scope.tracks(&copy) {
                            pushes.push((copy, mine));
                        }
                    }
                    conflicts.insert(path.clone());
                    let content = match theirs {
                        Some((object, content)) => {
                            base.insert(path.clone(), base_of(object, &content));
                            Some(content)
                        }
                        None => {
                            base.remove(&path);
                            None
                        }
                    };
                    applies.push(Apply {
                        path,
                        expected: Some(sha),
                        content,
                        pulled: false,
                    });
                }
            }
        }
        let wrote = !pushes.is_empty() || !dropped.is_empty();
        let mut revision = list.revision;
        let mut pushed = dropped.clone();
        let mut obsolete = Vec::new();
        if wrote {
            let mut sealed = Vec::new();
            for (path, plain) in &pushes {
                let (entry, object) = seal(&key, config, path, plain)?;
                base.insert(path.clone(), base_of(entry.sha256.clone(), plain));
                pushed.insert(path.clone());
                sealed.push((entry, object));
            }
            let replaced: HashSet<&str> = pushes
                .iter()
                .map(|(path, _)| path.as_str())
                .chain(dropped.iter().map(String::as_str))
                .collect();
            let mut entries = Vec::new();
            for (path, entry) in names.iter().zip(&list.entries) {
                if replaced.contains(path.as_str()) {
                    obsolete.push(entry.sha256.clone());
                } else {
                    entries.push(entry.clone());
                }
            }
            entries.extend(sealed.iter().map(|(entry, _)| entry.clone()));
            if entries.len() > MAX_ENTRIES {
                return Err(Error::new(
                    "Live list would exceed 2000 entries; move files out of the live roots",
                ));
            }
            revision = list
                .revision
                .checked_add(1)
                .ok_or_else(|| Error::new("Live list revision overflow"))?;
            let bytes = List {
                version: 1,
                revision,
                salt: list.salt.clone(),
                spec: list.spec.clone(),
                entries,
            }
            .encode()?;
            let size = sealed.iter().map(|(_, object)| object.len()).sum::<usize>() + bytes.len();
            budget::authorize(config, &[url(&config.repositories.bot)], size as u64)?;
            for (entry, object) in &sealed {
                put_object(&store, &entry.sha256, object)?;
            }
            if !put_list(&store, &bytes, Condition::Match(&token))? {
                continue 'attempt;
            }
        }
        return Ok(Some(Merged {
            wrote,
            revision,
            salt: list.salt,
            spec,
            base,
            applies,
            obsolete,
            pushed,
            conflicts,
        }));
    }
    Err(Error::new("Live list stayed in conflict; retry the check"))
}

/// Applies a merged list locally, then deletes objects only the replaced list referenced.
fn apply(
    config: &Config,
    owned: &Path,
    live: &mut Live,
    merged: Merged,
    report: &mut Report,
) -> Result<()> {
    let mut base = merged.base;
    for apply in merged.applies {
        // A file edited since the merge read it keeps the edit; the next check merges it.
        if disk(owned, &apply.path)?.sha() != apply.expected {
            match live.base.get(&apply.path) {
                Some(previous) => base.insert(apply.path, previous.clone()),
                None => base.remove(&apply.path),
            };
            continue;
        }
        match &apply.content {
            Some(content) => write_file(owned, &apply.path, content, apply.expected.is_some())?,
            None => remove_file(owned, &apply.path)?,
        }
        if apply.pulled {
            report.pulled.insert(apply.path);
        }
    }
    live.base = base;
    live.list = Some(Applied {
        revision: merged.revision,
        salt: merged.salt,
        spec: merged.spec,
    });
    report.pushed = merged.pushed;
    report.conflicts = merged.conflicts;
    let store = ObjectStore::new(config, url(&config.repositories.bot))?;
    for sha in &merged.obsolete {
        // A failed delete leaves an orphan that only costs storage.
        let _ = store.delete(&object_key(sha));
    }
    Ok(())
}

/// Records this machine's list revision and newer hub mark in the index, by CAS.
fn update_index(
    config: &Config,
    store: &ObjectStore,
    live: &mut Live,
    mut observed: Observed,
    cached: &HubMark,
) -> Result<()> {
    let key = bot_key(&config.hub, config.id());
    let hub = live
        .pending_hub
        .iter()
        .chain([cached])
        .max_by_key(|mark| mark.revision)
        .expect("a cached hub mark")
        .clone();
    for _ in 0..CAS_ATTEMPTS {
        // An unconditional read never answers NotModified.
        let current = match observed.take() {
            Some(current) => current,
            None => read_index(store, None)?.flatten(),
        };
        let settled = match next_index(
            current.as_ref().map(|(index, _)| index),
            &key,
            live.pending_index,
            &hub,
        ) {
            None => true,
            Some(next) => {
                let bytes = next.encode()?;
                budget::authorize(
                    config,
                    &[url(&config.repositories.shared)],
                    bytes.len() as u64,
                )?;
                // A write leaves no token to trust; the next check reads the index in full.
                live.index = None;
                let condition = match &current {
                    Some((_, token)) => Condition::Match(token),
                    None => Condition::Create,
                };
                // On a conflict or a lost response, the next round re-reads and sees the result.
                store.put(INDEX, &bytes, condition)? == Written::Yes
            }
        };
        if settled {
            live.pending_index = None;
            live.pending_hub = None;
            return Ok(());
        }
    }
    Err(Error::new(
        "Live index stayed in conflict; the next check retries the update",
    ))
}

/// The index with this machine's updates, or `None` when it needs no write.
fn next_index(
    current: Option<&Index>,
    key: &str,
    revision: Option<u64>,
    hub: &HubMark,
) -> Option<Index> {
    let Some(current) = current else {
        // The first list writer creates the index; a hub repair alone never does.
        return revision.map(|revision| Index {
            version: 1,
            hub: hub.clone(),
            bots: BTreeMap::from([(key.to_owned(), revision)]),
        });
    };
    let mut next = current.clone();
    if let Some(revision) = revision {
        let listed = next.bots.entry(key.to_owned()).or_insert(revision);
        *listed = (*listed).max(revision);
    }
    if hub.revision > next.hub.revision {
        next.hub = hub.clone();
    }
    (next != *current).then_some(next)
}

pub fn define(config: &Config, tasks: &[String], rules: &[String], policy: Policy) -> Result<()> {
    config.runtime()?;
    let owned = owned(config);
    let globs = exclusions(config)?;
    let roots = tasks
        .iter()
        .map(|path| (path, Class::Task))
        .chain(rules.iter().map(|path| (path, Class::Rule)))
        .map(|(path, class)| {
            Ok(Root {
                path: root_path(config, owned, &globs, path)?,
                class,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let spec = Spec { roots, policy };
    spec.validate()?;
    let root = state_root(config);
    let _lock = Lock::acquire(root)?;
    let mut live = Live::load(root)?.unwrap_or_default();
    let cached = joined(config)?;
    let store = ObjectStore::new(config, url(&config.repositories.bot))?;
    let password = password(&config.repositories.bot)?;
    for _ in 0..CAS_ATTEMPTS {
        let current = read_list(&store)?;
        let mut obsolete = Vec::new();
        let next = match &current {
            None => {
                let salt = crypto::salt()?;
                let key = crypto::derive(password.as_encoded_bytes(), &salt)?;
                List {
                    version: 1,
                    revision: 1,
                    salt: hex(&salt),
                    spec: seal_spec(&key, config, &spec)?,
                    entries: Vec::new(),
                }
            }
            Some((list, _)) => {
                let key = derive(&password, list)?;
                open_spec(&key, config, list).map_err(|_| {
                    Error::new(
                        "Live list cannot be decrypted with this repository password; delete uah-live/ from storage and define again",
                    )
                })?;
                let mut entries = Vec::new();
                for (path, entry) in open_names(&key, config, list)?.iter().zip(&list.entries) {
                    if spec.roots.iter().any(|root| covers(&root.path, path)) {
                        entries.push(entry.clone());
                    } else {
                        obsolete.push(entry.sha256.clone());
                    }
                }
                List {
                    version: 1,
                    revision: list
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::new("Live list revision overflow"))?,
                    salt: list.salt.clone(),
                    spec: seal_spec(&key, config, &spec)?,
                    entries,
                }
            }
        };
        let bytes = next.encode()?;
        budget::authorize(config, &[url(&config.repositories.bot)], bytes.len() as u64)?;
        let condition = match &current {
            None => Condition::Create,
            Some((_, token)) => Condition::Match(token),
        };
        if !put_list(&store, &bytes, condition)? {
            continue;
        }
        live.pending_index = Some(next.revision);
        // New roots sync at the next scheduled call rather than after the previous interval.
        live.next_check = live.next_check.min(Utc::now().timestamp());
        live.save(root)?;
        let shared = ObjectStore::new(config, url(&config.repositories.shared))?;
        update_index(config, &shared, &mut live, None, &cached)?;
        live.save(root)?;
        for sha in &obsolete {
            let _ = store.delete(&object_key(sha));
        }
        println!("{}", json!({"defined": true, "revision": next.revision}));
        return Ok(());
    }
    Err(Error::new(
        "Live list stayed in conflict; retry live define",
    ))
}

/// Checks one `live define` path and returns it as a list root: `dir/` or `file`.
fn root_path(config: &Config, owned: &Path, globs: &GlobSet, given: &str) -> Result<String> {
    let invalid = |reason: &str| Error::new(format!("Live root {given:?} {reason}"));
    relative_path(given).map_err(|_| invalid("must be a relative path under paths.owned"))?;
    let path = given.strip_suffix('/').unwrap_or(given);
    let full = owned.join(path);
    no_symlinks(&full).map_err(|_| invalid("must not be or pass through a symlink"))?;
    let metadata =
        fs::symlink_metadata(&full).map_err(|_| invalid("does not exist under paths.owned"))?;
    if excluded(Path::new(path), globs) {
        return Err(invalid("is excluded from Bot data"));
    }
    // The primary's daily backup must hold every live file.
    if !config.data.include.iter().any(|include| {
        let include: PathBuf = include
            .components()
            .filter(|part| *part != Component::CurDir)
            .collect();
        Path::new(path).starts_with(include)
    }) {
        return Err(invalid("must lie inside data.include"));
    }
    if metadata.is_dir() {
        Ok(format!("{path}/"))
    } else if metadata.is_file() {
        Ok(path.to_owned())
    } else {
        Err(invalid("must be a regular file or directory"))
    }
}

/// Whether a list root (`dir/` or `file`) covers `path`.
fn covers(root: &str, path: &str) -> bool {
    if root.ends_with('/') {
        path.starts_with(root)
    } else {
        path == root
    }
}

fn read_index(store: &ObjectStore, token: Option<&str>) -> Result<Observed> {
    Ok(match store.get(INDEX, token, MAX_BYTES)? {
        Fetched::NotModified => None,
        Fetched::Missing => Some(None),
        Fetched::Found { body, token } => Some(Some((Index::decode(&body)?, token))),
    })
}

fn read_list(store: &ObjectStore) -> Result<Option<(List, String)>> {
    match store.get(LIST, None, MAX_BYTES)? {
        Fetched::Found { body, token } => Ok(Some((List::decode(&body)?, token))),
        Fetched::Missing => Ok(None),
        Fetched::NotModified => Err(not_modified()),
    }
}

/// Writes the list under `condition`; `false` when another writer won.
fn put_list(store: &ObjectStore, bytes: &[u8], condition: Condition<'_>) -> Result<bool> {
    Ok(match store.put(LIST, bytes, condition)? {
        Written::Yes => true,
        Written::Conflict => false,
        // The response was lost: the write landed only if the list now holds these bytes.
        Written::Uncertain => matches!(
            store.get(LIST, None, MAX_BYTES)?,
            Fetched::Found { body, .. } if body == bytes
        ),
    })
}

fn put_object(store: &ObjectStore, sha: &str, object: &[u8]) -> Result<()> {
    let key = object_key(sha);
    match store.put(&key, object, Condition::Create)? {
        // Keys are content hashes: an existing object already holds these bytes.
        Written::Yes | Written::Conflict => Ok(()),
        Written::Uncertain => match store.get(&key, None, MAX_OBJECT_BYTES)? {
            Fetched::Found { body, .. } if body == object => Ok(()),
            _ => Err(Error::new(
                "Live object upload was not confirmed; retry the check",
            )),
        },
    }
}

/// The verified plaintext of a listed object, or `None` once it has been deleted.
fn download(store: &ObjectStore, key: &Key, config: &Config, sha: &str) -> Result<Option<Vec<u8>>> {
    match store.get(&object_key(sha), None, MAX_OBJECT_BYTES)? {
        Fetched::Found { body, .. } if sha256_hex(&body) == sha => {
            crypto::open(key, &config.hub, config.id(), Kind::Object, None, &body).map(Some)
        }
        Fetched::Found { .. } => Err(Error::new("Live object does not match its listed SHA-256")),
        Fetched::Missing => Ok(None),
        Fetched::NotModified => Err(not_modified()),
    }
}

fn derive(password: &OsStr, list: &List) -> Result<Key> {
    let salt: [u8; SALT_BYTES] = unhex(&list.salt)?
        .try_into()
        .map_err(|_| Error::new("Invalid live list"))?;
    crypto::derive(password.as_encoded_bytes(), &salt)
}

fn open_spec(key: &Key, config: &Config, list: &List) -> Result<Spec> {
    let plain = crypto::open(
        key,
        &config.hub,
        config.id(),
        Kind::Spec,
        None,
        &unhex(&list.spec)?,
    )?;
    let spec: Spec = serde_json::from_slice(&plain).map_err(|_| Error::new("Invalid live list"))?;
    spec.validate()?;
    Ok(spec)
}

fn seal_spec(key: &Key, config: &Config, spec: &Spec) -> Result<String> {
    let plain = serde_json::to_vec(spec).map_err(|_| Error::new("Cannot encode live spec"))?;
    Ok(hex(&crypto::seal(
        key,
        &config.hub,
        config.id(),
        Kind::Spec,
        None,
        &plain,
    )?))
}

/// Decrypts every entry name; each must be a distinct relative file path.
fn open_names(key: &Key, config: &Config, list: &List) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    list.entries
        .iter()
        .map(|entry| {
            let plain = crypto::open(
                key,
                &config.hub,
                config.id(),
                Kind::Name,
                Some(&entry.sha256),
                &unhex(&entry.name)?,
            )?;
            let path = String::from_utf8(plain).map_err(|_| Error::new("Invalid live list"))?;
            relative_path(&path)?;
            if path.ends_with('/') || !seen.insert(path.clone()) {
                return Err(Error::new("Invalid live list"));
            }
            Ok(path)
        })
        .collect()
}

/// Seals a file as an object and its name bound to that object's SHA-256.
fn seal(key: &Key, config: &Config, path: &str, plain: &[u8]) -> Result<(Entry, Vec<u8>)> {
    let object = crypto::seal(key, &config.hub, config.id(), Kind::Object, None, plain)?;
    let sha256 = sha256_hex(&object);
    let name = hex(&crypto::seal(
        key,
        &config.hub,
        config.id(),
        Kind::Name,
        Some(&sha256),
        path.as_bytes(),
    )?);
    Ok((Entry { name, sha256 }, object))
}

/// The conflict copy of `path` holding `sha`: an existing copy of those bytes (`false`), or
/// the first free name (`true`). A taken name gets a number after the SHA-256 prefix.
fn conflict_copy(
    owned: &Path,
    path: &str,
    sha: &str,
    listed: &BTreeMap<String, String>,
    base: &BTreeMap<String, Base>,
) -> Result<(String, bool)> {
    let first = merge::conflict_name(path, sha);
    let marker = format!(".conflict-{}", &sha[..8]);
    let split = first.rfind(&marker).expect("conflict name marker") + marker.len();
    let mut number = 1;
    loop {
        let name = if number == 1 {
            first.clone()
        } else {
            format!("{}-{number}{}", &first[..split], &first[split..])
        };
        relative_path(&name)?;
        match disk(owned, &name)? {
            Disk::File(plain) if sha256_hex(&plain) == sha => return Ok((name, false)),
            Disk::Absent if !listed.contains_key(&name) && !base.contains_key(&name) => {
                return Ok((name, true));
            }
            _ => number += 1,
        }
    }
}

/// The local file at `relative`, read without following symlinks.
fn disk(owned: &Path, relative: &str) -> Result<Disk> {
    let path = owned.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(Disk::Absent);
        }
        Err(error) => {
            return Err(Error::new(format!(
                "Cannot inspect live file {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.is_file() {
        return Ok(Disk::Other);
    }
    let too_large = || {
        Error::new(format!(
            "Live file exceeds the 1 MiB limit; move it out of the live roots: {}",
            path.display()
        ))
    };
    if metadata.len() > MAX_PLAINTEXT {
        return Err(too_large());
    }
    let mut plain = Vec::new();
    open_source(owned, Path::new(relative))?
        .take(MAX_PLAINTEXT + 1)
        .read_to_end(&mut plain)
        .at("Cannot read live file", &path)?;
    if plain.len() as u64 > MAX_PLAINTEXT {
        return Err(too_large());
    }
    Ok(Disk::File(plain))
}

/// Writes a live file atomically. A new file is 0600 and never replaces one created meanwhile;
/// a replaced file keeps its mode.
fn write_file(owned: &Path, relative: &str, content: &[u8], replace: bool) -> Result<()> {
    let path = owned.join(relative);
    let parent = path.parent().expect("a live path has a parent");
    no_symlinks(&path)?;
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories
        .create(parent)
        .at("Cannot create live directory", parent)?;
    let mut staged = tempfile::Builder::new()
        .prefix(STAGING)
        .tempfile_in(parent)
        .at("Cannot stage live file in", parent)?;
    staged
        .write_all(content)
        .at("Cannot write live file", &path)?;
    if replace
        && let Ok(metadata) = fs::symlink_metadata(&path)
        && metadata.is_file()
    {
        staged
            .as_file()
            .set_permissions(metadata.permissions())
            .at("Cannot keep live file mode", &path)?;
    }
    staged
        .as_file()
        .sync_all()
        .at("Cannot sync live file", &path)?;
    let persisted = if replace {
        staged.persist(&path)
    } else {
        staged.persist_noclobber(&path)
    };
    persisted.map_err(|error| {
        Error::new(format!(
            "Cannot write live file {}: {}",
            path.display(),
            error.error
        ))
    })?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .at("Cannot sync live directory", parent)
}

fn remove_file(owned: &Path, relative: &str) -> Result<()> {
    let path = owned.join(relative);
    no_symlinks(&path)?;
    match fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(Error::new(format!(
            "Cannot remove live file {}: {error}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

/// The base entry of a path whose object `object` holds `plain`.
fn base_of(object: String, plain: &[u8]) -> Base {
    Base {
        object,
        plain: sha256_hex(plain),
    }
}

fn object_key(sha: &str) -> String {
    format!("uah-live/objects/{sha}")
}

fn url(repository: &Repository) -> &str {
    repository.url.as_deref().expect("runtime configuration")
}

fn owned(config: &Config) -> &Path {
    config
        .paths
        .owned
        .as_deref()
        .expect("runtime configuration")
}

fn state_root(config: &Config) -> &Path {
    config
        .paths
        .state
        .as_deref()
        .expect("runtime configuration")
}

fn missing_cache() -> Error {
    Error::new("Shared cache is missing after import; retry the live command")
}

fn not_modified() -> Error {
    Error::new("Live storage answered an unconditional read with 304")
}

/// A uniform sample from [0, 1) for jitter.
fn unit() -> f64 {
    // Jitter only spreads load; without OS randomness every check keeps the midpoint.
    u64::try_generate().map_or(0.5, |bits| (bits >> 11) as f64 / (1u64 << 53) as f64)
}

/// The failure as `status` shows it: one bounded line, never a child diagnostic.
fn sanitized(message: &str) -> String {
    message
        .split("; stderr:")
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(500)
        .collect()
}
