use crate::config::{Config, Repository, absolute, identity, no_symlinks};
use crate::decisions::{self, Decisions};
use crate::error::{Error, IoContext, Result};
use crate::head::{Head, HeadStore, ObservedHead};
use crate::history::History;
use crate::live::format::HubMark;
use crate::live::policy::Policy;
use crate::live::state::Live;
use crate::registry::{LegacyRegistry, Registry, Role};
use crate::restic::{BackupGuard, Restic, Snapshot, latest, password};
use crate::selection::{self, Export};
use crate::shared::{self, SharedCache};
use crate::state::{self, Failure, Lock, State, fresh_destination, private_dir, private_file};
use crate::{Action, LiveAction, RegistryAction, RepositoryKind, budget, live, migration};
use crate::{retention, schedule};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

const REVISION_FILE: &str = "shared-revision.json";
const CAS_ATTEMPTS: usize = 16;

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Revision {
    version: u32,
    hub: String,
    repository_sha256: String,
    revision: u64,
}

impl Revision {
    fn new(config: &Config, revision: u64) -> Self {
        Self {
            version: 2,
            hub: config.hub.clone(),
            repository_sha256: decisions::repository_sha256(config),
            revision,
        }
    }

    fn load(root: &Path, config: &Config) -> Result<Self> {
        let path = root.join(REVISION_FILE);
        no_symlinks(&path)?;
        let invalid = || {
            Error::new(
                "Missing or invalid authoring revision base; copy verified active_shared, omit generated registry-history.json, and reconcile edits before publishing",
            )
        };
        let bytes = fs::read(path).map_err(|_| invalid())?;
        let revision: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if revision.revision == 0 || revision != Self::new(config, revision.revision) {
            return Err(invalid());
        }
        Ok(revision)
    }

    fn write(&self, export: &mut Export) -> Result<()> {
        export.write(
            REVISION_FILE,
            &serde_json::to_vec(self)
                .map_err(|_| Error::new("Cannot encode shared revision base"))?,
        )
    }
}

struct Prepared {
    registry: Registry,
    history: History,
    export: Export,
}

struct ImportedData {
    registry: Registry,
    history: History,
}

struct Imported {
    data: ImportedData,
    observed: ObservedHead,
    cache: SharedCache,
}

impl Imported {
    fn prepare(&self, config: &Config, restic: &Restic<'_>) -> Result<Prepared> {
        let id = &self.observed.head.snapshot;
        let export = self.cache.load(
            id,
            |path| restic.restore(&config.repositories.shared, id, path),
            |path| selection::copy_verified(config, path),
        )?;
        Ok(Prepared {
            registry: self.data.registry.clone(),
            history: self.data.history.clone(),
            export,
        })
    }

    fn publish(self) -> Result<()> {
        self.cache.publish(&self.observed.head.snapshot)
    }
}

pub fn execute(path: &Path, action: Action) -> Result<()> {
    let config = Config::load(path)?;
    match action {
        Action::Validate { template } => {
            if !template {
                config.runtime()?;
            }
            println!(
                "{}",
                serde_json::json!({"valid": true, "ready": config.missing().is_empty()})
            );
            return Ok(());
        }
        Action::Status => return status(&config),
        Action::Live { command } => return live_action(&config, command),
        _ => (),
    }
    let operation = match &action {
        Action::Setup => "setup",
        Action::Sync { .. } => "sync",
        Action::Registry { .. } => "publish",
        Action::Pull { .. } => "pull",
        Action::Restore { .. } => "restore",
        Action::Prune => "prune",
        _ => unreachable!(),
    };
    let Some(root) = config.paths.state.as_ref() else {
        return config.runtime();
    };
    let _lock = Lock::acquire(root)?;
    let mut state = State::load(root, &config.hub)?;
    // A skipped scheduled operation must leave every unresolved failure intact.
    if matches!(action, Action::Sync { scheduled: true })
        && state
            .next_due
            .is_some_and(|due| Utc::now().timestamp() < due)
    {
        println!("{}", serde_json::json!({"skipped": true}));
        return Ok(());
    }
    let writes = matches!(
        action,
        Action::Setup | Action::Sync { .. } | Action::Registry { .. } | Action::Prune
    );
    let previous = state.clone();
    let result = (|| {
        config.runtime()?;
        if writes {
            state::require_reconciled(root)?;
        }
        private_dir(
            config
                .paths
                .imports
                .as_ref()
                .expect("runtime configuration"),
        )?;
        let restic = Restic::open(&config, matches!(action, Action::Setup))?;
        match action {
            Action::Setup => setup(&config, &restic, &mut state),
            Action::Sync { scheduled } => sync(&config, &restic, &mut state, scheduled),
            Action::Registry {
                command: RegistryAction::Publish { takeover },
            } => publish(&config, &restic, takeover),
            Action::Pull {
                bot_id,
                destination,
                allow_identity_mismatch,
            } => pull(
                &config,
                &restic,
                &bot_id,
                &destination,
                allow_identity_mismatch,
            ),
            Action::Restore {
                repository,
                snapshot,
                destination,
            } => {
                let repository = match repository {
                    RepositoryKind::Shared => &config.repositories.shared,
                    RepositoryKind::Bot => &config.repositories.bot,
                };
                recover(&config, &restic, repository, &snapshot, &destination)
            }
            Action::Prune => prune(&config, &restic),
            Action::Validate { .. } | Action::Status | Action::Live { .. } => unreachable!(),
        }
    })();
    if let Err(error) = result {
        state = previous;
        state
            .failures
            .insert(operation.to_owned(), Failure::sanitized(operation, &error));
        // Preserve the original error if recording status itself fails.
        let _ = state.save(root);
        return Err(error);
    }
    state.failures.remove(operation);
    state.save(root)?;
    if writes && state::pending_backup(root).try_exists()? {
        state::finish_backup(root)?;
    }
    println!("Operation completed");
    Ok(())
}

fn live_action(config: &Config, command: LiveAction) -> Result<()> {
    match command {
        LiveAction::Check => live::sync::check(config),
        LiveAction::Define {
            tasks,
            rules,
            base_seconds,
            idle_after_seconds,
            task_idle_seconds,
            rule_idle_seconds,
        } => {
            let default = Policy::default();
            let policy = Policy {
                base_seconds: base_seconds.unwrap_or(default.base_seconds),
                idle_after_seconds: idle_after_seconds.unwrap_or(default.idle_after_seconds),
                task_idle_seconds: task_idle_seconds.unwrap_or(default.task_idle_seconds),
                rule_idle_seconds: rule_idle_seconds.unwrap_or(default.rule_idle_seconds),
            };
            live::sync::define(config, &tasks, &rules, policy)
        }
    }
}

pub(crate) fn status_time(seconds: Option<i64>) -> Result<Option<String>> {
    seconds
        .map(|seconds| {
            let time = DateTime::from_timestamp(seconds, 0)
                .filter(|time| (0..=9999).contains(&time.year()))
                .ok_or_else(|| Error::new("Status timestamp out of range for RFC3339"))?;
            Ok(time.to_rfc3339_opts(SecondsFormat::Secs, true))
        })
        .transpose()
}

fn status(config: &Config) -> Result<()> {
    let state = if let Some(root) = &config.paths.state {
        State::load(root, &config.hub)?
    } else {
        State::default()
    };
    let missing = config.missing();
    let active_shared = shared::active(config)?;
    let role = active_shared
        .as_ref()
        .and_then(|path| Registry::load(path, config).ok())
        .and_then(|registry| registry.role(config));
    let live = match &config.paths.state {
        Some(root) => Live::load(root)?,
        None => None,
    };
    let live = match live {
        Some(live) => serde_json::json!({
            "next_check": status_time(Some(live.next_check))?, "failure": live.failure,
        }),
        None => serde_json::Value::Null,
    };
    let failure = state.failures.get("setup").or_else(|| {
        state
            .failures
            .values()
            .max_by_key(|failure| failure.recorded_at)
    });
    let (next_action, remediation) = if !missing.is_empty() {
        (
            "configure",
            missing
                .iter()
                .map(|field| format!("Set {field} in configuration"))
                .collect::<Vec<_>>(),
        )
    } else if let Some(root) = &config.paths.state
        && state::require_reconciled(root).is_err()
    {
        (
            "reconcile",
            vec![format!(
                "Reconcile the backup recorded in {} before another write",
                state::pending_backup(root).display()
            )],
        )
    } else if let Some(failure) = failure {
        (
            failure.operation.as_str(),
            vec![failure.reason.clone(), failure.next_action.clone()],
        )
    } else if role == Some(Role::Secondary) {
        (
            "live",
            vec![
                "Secondary runtime: run live check; the primary runtime backs up Bot data"
                    .to_owned(),
            ],
        )
    } else if !state.joined || state.anchor.is_none() || state.next_due.is_none() {
        (
            "setup",
            vec![
                "Run setup to register with the shared hub and initialize the schedule".to_owned(),
            ],
        )
    } else if active_shared.is_none() {
        (
            "sync",
            vec!["Run sync to restore the accepted shared head into the shared cache and back up the selected Bot data".to_owned()],
        )
    } else if state
        .next_due
        .is_some_and(|due| due <= Utc::now().timestamp())
    {
        ("sync", Vec::new())
    } else {
        ("wait", Vec::new())
    };
    println!(
        "{}",
        serde_json::json!({
            "hub": config.hub, "ready": missing.is_empty(), "joined": state.joined,
            "last_success": status_time(state.last_success)?, "anchor": status_time(state.anchor)?,
            "next_due": status_time(state.next_due)?, "active_shared": active_shared,
            "failure": failure, "next_action": next_action, "remediation": remediation,
            "machine": config.machine_hash(), "live": live,
            "role": role.map(|role| match role {
                Role::Primary => "primary",
                Role::Secondary => "secondary",
            }),
        })
    );
    Ok(())
}

/// The hub head this machine last imported: its revision and snapshot.
pub(crate) fn hub_mark(config: &Config) -> Result<Option<HubMark>> {
    let Some(path) = shared::active(config)? else {
        return Ok(None);
    };
    let snapshot = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("shared version directory")
        .to_owned();
    Ok(Some(HubMark {
        revision: Revision::load(&path, config)?.revision,
        snapshot,
    }))
}

/// Imports the accepted shared head into the shared cache and makes it current.
pub(crate) fn import_hub(config: &Config) -> Result<()> {
    private_dir(
        config
            .paths
            .imports
            .as_ref()
            .expect("runtime configuration"),
    )?;
    let restic = Restic::open(config, false)?;
    import_shared(config, &restic)?.publish()
}

fn import_shared(config: &Config, restic: &Restic<'_>) -> Result<Imported> {
    password(&config.repositories.shared)?;
    let cache = SharedCache::open(config)?;
    let observed = HeadStore::new(config)?.read()?.ok_or_else(|| {
        Error::new("No accepted shared head exists; run setup to initialize or migrate the hub")
    })?;
    // Inventory validation still rejects foreign data and malformed controls.
    // Only the exact head target can become current; candidates and v1 latest cannot.
    let snapshots = restic.snapshots(&config.repositories.shared)?;
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot.id == observed.head.snapshot)
        .ok_or_else(|| {
            Error::new("Accepted shared head snapshot is missing; explicit recovery is required")
        })?;
    if !snapshot.kind("shared-v2")
        || snapshot.tags.len() != 6
        || snapshot.tag("hub:") != Some(config.hub.as_str())
        || snapshot.tag("bot:") != Some(snapshot.hostname.as_str())
        || !identity(&snapshot.hostname)
        || !snapshot.tag("runtime:").is_some_and(identity)
        || !snapshot.tag("run:").is_some_and(identity)
        || snapshot.tag("kind:") != Some("shared-v2")
        || DateTime::parse_from_rfc3339(&snapshot.time).is_err()
    {
        return Err(Error::new(
            "Accepted shared snapshot source identity is invalid",
        ));
    }
    let data = cache.load(
        &snapshot.id,
        |path| restic.restore(&config.repositories.shared, &snapshot.id, path),
        |path| {
            selection::validate_import(path)?;
            let registry = Registry::load(path, config)?;
            let history = History::load(path, &registry)?;
            Decisions::load(path, config)?;
            if Revision::load(path, config)?.revision != observed.head.revision {
                return Err(Error::new(
                    "Shared snapshot revision does not match its accepted head",
                ));
            }
            Ok(ImportedData { registry, history })
        },
    )?;
    Ok(Imported {
        data,
        observed,
        cache,
    })
}

fn initialize_schedule(config: &Config, state: &mut State) -> Result<()> {
    if state.anchor.is_none() {
        let anchor = schedule::first_anchor(
            Utc::now().timestamp(),
            config.timezone.as_deref().expect("runtime configuration"),
            &config.sync.start_time,
        )?;
        state.anchor = Some(anchor);
        state.next_due = Some(anchor);
    }
    if state.next_due.is_none() {
        return Err(Error::new("Persisted schedule is incomplete"));
    }
    Ok(())
}

fn private_repository(config: &Config, restic: &Restic<'_>, history: &History) -> Result<()> {
    if !config.sharing.bot_data {
        history.check_private(&config.repositories.bot, restic)?;
        let existing_id = |repository| match restic.repository_id(repository) {
            Ok(id) => Ok(Some(id)),
            Err(error) if error.code == 10 => Ok(None),
            Err(error) => Err(error),
        };
        let own = existing_id(&config.repositories.bot)?;
        let shared = existing_id(&config.repositories.shared)?;
        if own.is_some() && own == shared {
            return Err(Error::new(
                "Private Bot data requires a separate repository and master key",
            ));
        }
    }
    Ok(())
}

fn shared_budget(config: &Config, bytes: u64) -> Result<()> {
    budget::authorize(
        config,
        &[
            config
                .repositories
                .shared
                .url
                .as_deref()
                .expect("runtime configuration"),
            config
                .repositories
                .bot
                .url
                .as_deref()
                .expect("runtime configuration"),
        ],
        bytes,
    )
}

fn rewrite_metadata(config: &Config, data: &mut Prepared, revision: u64) -> Result<()> {
    data.registry.write(&mut data.export)?;
    data.export.remove("registry-history.json")?;
    data.history.write(&mut data.export)?;
    Revision::new(config, revision).write(&mut data.export)
}

// This callback is also run on the retained capture after a durable-fence restart.
// Its complete candidate and byte budget are checked before the fence is written.
fn prepare_initial(
    config: &Config,
    restic: &Restic<'_>,
    source: Option<&Path>,
    legacy_retry: bool,
) -> Result<Prepared> {
    let mut data = if let Some(source) = source {
        let registry = LegacyRegistry::load(source, config)?.convert(config)?;
        let history = History::load(source, &registry)?;
        let mut export = selection::copy_verified(config, source)?;
        decisions::supplement(&mut export, config)?;
        Prepared {
            registry,
            history,
            export,
        }
    } else {
        let root = config.paths.shared.as_ref().expect("runtime configuration");
        let registry = Registry::initial(root, config, legacy_retry)?;
        Decisions::load(root, config)?;
        let export = selection::export(config, true)?;
        if export
            .tree
            .path()
            .join("registry-history.json")
            .try_exists()?
        {
            return Err(Error::new(
                "registry-history.json is reserved generated shared metadata",
            ));
        }
        Prepared {
            registry,
            history: History::new(),
            export,
        }
    };
    Decisions::load(data.export.tree.path(), config)?;
    data.registry.register(config)?;
    data.registry.credentials(config)?;
    data.history.observe(&data.registry, config, restic, true)?;
    data.history.check(&data.registry, restic)?;
    private_repository(config, restic, &data.history)?;
    rewrite_metadata(config, &mut data, 1)?;
    // Reserve the fixed-width master IDs of repositories that setup is about to
    // initialize. The callback after ensure's initialization uses actual IDs.
    let mut pending_ids = 0u64;
    for repository in std::iter::once(&config.repositories.shared)
        .chain(config.sharing.bot_data.then_some(&config.repositories.bot))
    {
        match restic.repository_id(repository) {
            Err(error) if error.code == 10 => pending_ids += 1,
            Err(error) => return Err(error),
            Ok(_) => (),
        }
    }
    shared_budget(
        config,
        data.export
            .bytes
            .checked_add(data.history.pending_master_bytes(pending_ids))
            .ok_or_else(|| Error::new("Source size overflow"))?,
    )?;
    Ok(data)
}

fn write_authoring(config: &Config, name: &str, bytes: &[u8]) -> Result<()> {
    let root = config.paths.shared.as_ref().expect("runtime configuration");
    no_symlinks(root)?;
    let path = root.join(name);
    no_symlinks(&path)?;
    let mut temp = tempfile::NamedTempFile::new_in(root)?;
    private_file(temp.as_file(), &path)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|_| {
        Error::new(
            "Cannot refresh authoring metadata; preserve edits and copy the accepted current base",
        )
    })?;
    File::open(root)?.sync_all()?;
    Ok(())
}

fn commit_candidate(
    config: &Config,
    restic: &Restic<'_>,
    store: &HeadStore,
    expected: Option<&ObservedHead>,
    data: &Prepared,
    guard: BackupGuard,
) -> Result<bool> {
    let revision = expected
        .map_or(Some(1), |head| head.head.revision.checked_add(1))
        .ok_or_else(|| Error::new("Shared revision overflow"))?;
    let snapshot = restic.backup_uncommitted(
        &config.repositories.shared,
        &data.export,
        "shared-v2",
        guard,
    )?;
    store.compare_exchange(
        expected,
        &Head {
            version: 2,
            hub: config.hub.clone(),
            revision,
            snapshot,
        },
    )
}

fn setup(config: &Config, restic: &Restic<'_>, state: &mut State) -> Result<()> {
    password(&config.repositories.shared)?;
    password(&config.repositories.bot)?;
    let owned = selection::export(config, false)?;
    budget::authorize(
        config,
        &[config
            .repositories
            .bot
            .url
            .as_deref()
            .expect("runtime configuration")],
        owned.bytes,
    )?;
    drop(owned);
    let snapshots = match restic.snapshots(&config.repositories.shared) {
        Ok(snapshots) => Some(snapshots),
        Err(error) if error.code == 10 => None,
        Err(error) => return Err(error),
    };
    let store = HeadStore::new(config)?;
    let can_read_head = config.storage.provider != "local"
        || Path::new(
            config
                .repositories
                .shared
                .url
                .as_deref()
                .expect("runtime configuration"),
        )
        .try_exists()?;
    let head = if can_read_head { store.read()? } else { None };
    if head.is_none() {
        let legacy_retry = snapshots.as_ref().is_some_and(|snapshots| {
            latest(snapshots, "shared").is_none()
                && !snapshots
                    .iter()
                    .any(|snapshot| snapshot.tags.iter().any(|tag| tag.starts_with("uah2:")))
                && snapshots.iter().any(|snapshot| snapshot.kind("shared"))
        });
        // Validate the entire local initial input before ensure can initialize or
        // fence an empty repository. Legacy/capture input is checked by callback.
        if snapshots.as_ref().is_none_or(|snapshots| {
            latest(snapshots, "shared").is_none()
                && !snapshots
                    .iter()
                    .any(|snapshot| snapshot.tags.iter().any(|tag| tag.starts_with("uah2:")))
        }) {
            drop(prepare_initial(config, restic, None, legacy_retry)?);
        }
        let mut prepared = None;
        let mut fence = None;
        for _ in 0..CAS_ATTEMPTS {
            let result = migration::ensure(config, restic, &mut |source| {
                let mut data = prepare_initial(config, restic, source, legacy_retry)?;
                restic.initialize(&config.repositories.bot)?;
                data.history.observe(&data.registry, config, restic, true)?;
                data.history.check(&data.registry, restic)?;
                private_repository(config, restic, &data.history)?;
                rewrite_metadata(config, &mut data, 1)?;
                shared_budget(config, data.export.bytes)?;
                prepared = Some(data);
                Ok(())
            });
            match result {
                Ok(value) => {
                    fence = Some(value);
                    break;
                }
                Err(error)
                    if error.message.contains("source changed")
                        || error
                            .message
                            .contains("repository changed before initial migration") =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        let fence =
            fence.ok_or_else(|| Error::new("Migration source changed repeatedly; retry setup"))?;
        let data =
            prepared.ok_or_else(|| Error::new("Migration candidate preflight did not complete"))?;
        if commit_candidate(
            config,
            restic,
            &store,
            None,
            &data,
            BackupGuard::InitialSetup,
        )? {
            // Only the successful initial author's content receives this base.
            // A joiner or a losing initializer never relabels local content.
            if fence.source.is_none() {
                write_authoring(
                    config,
                    "registry.yaml",
                    &fs::read(data.export.tree.path().join("registry.yaml"))?,
                )?;
                write_authoring(
                    config,
                    REVISION_FILE,
                    &fs::read(data.export.tree.path().join(REVISION_FILE))?,
                )?;
            }
        }
    }
    for _ in 0..CAS_ATTEMPTS {
        let mut imported = import_shared(config, restic)?;
        let mut data = imported.prepare(config, restic)?;
        let changed = data.registry.register(config)?;
        data.registry.credentials(config)?;
        data.history.check(&data.registry, restic)?;
        private_repository(config, restic, &data.history)?;
        // No Bot repository is initialized until current identity/history checks
        // and the prospective shared candidate budget have all passed.
        let previous_history = data.history.clone();
        data.history.observe(&data.registry, config, restic, true)?;
        let revision = imported
            .observed
            .head
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::new("Shared revision overflow"))?;
        if changed || previous_history != data.history {
            rewrite_metadata(config, &mut data, revision)?;
            shared_budget(config, data.export.bytes)?;
        }
        restic.initialize(&config.repositories.bot)?;
        data.history.observe(&data.registry, config, restic, true)?;
        data.history.check(&data.registry, restic)?;
        private_repository(config, restic, &data.history)?;
        let history_changed = previous_history != data.history;
        let mut predecessors = Vec::new();
        if changed || history_changed {
            rewrite_metadata(config, &mut data, revision)?;
            shared_budget(config, data.export.bytes)?;
            if !commit_candidate(
                config,
                restic,
                &store,
                Some(&imported.observed),
                &data,
                BackupGuard::Persistent,
            )? {
                state::finish_backup(config.paths.state.as_ref().expect("runtime configuration"))?;
                continue;
            }
            predecessors.push(imported.observed.head.snapshot.clone());
            imported = import_shared(config, restic)?;
            imported.data.registry.current(config)?;
        }
        initialize_schedule(config, state)?;
        imported.publish()?;
        cleanup_predecessors(config, restic, &config.repositories.shared, predecessors)?;
        state.joined = true;
        return Ok(());
    }
    Err(Error::new(
        "Shared registration remained in conflict; retry setup; local work was preserved",
    ))
}

fn sync(config: &Config, restic: &Restic<'_>, state: &mut State, scheduled: bool) -> Result<()> {
    let anchor = state
        .anchor
        .ok_or_else(|| Error::new("Run setup to initialize the schedule"))?;
    if state.next_due.is_none() {
        return Err(Error::new("Run setup to initialize the schedule"));
    }
    let imported = import_shared(config, restic)?;
    imported.data.registry.current(config)?;
    imported.data.registry.credentials(config)?;
    imported
        .data
        .history
        .check(&imported.data.registry, restic)?;
    private_repository(config, restic, &imported.data.history)?;
    let export = selection::export(config, false)?;
    budget::authorize(
        config,
        &[config
            .repositories
            .bot
            .url
            .as_deref()
            .expect("runtime configuration")],
        export.bytes,
    )?;
    let predecessors = restic.backup(
        &config.repositories.bot,
        &export,
        "bot",
        BackupGuard::Persistent,
    )?;
    let completed = Utc::now().timestamp();
    state.last_success = Some(completed);
    if scheduled {
        state.next_due = Some(schedule::next_after(
            anchor,
            config.sync.interval_seconds,
            completed,
        )?);
    }
    imported.publish()?;
    cleanup_predecessors(config, restic, &config.repositories.bot, predecessors)?;
    state.joined = true;
    Ok(())
}

fn publish(config: &Config, restic: &Restic<'_>, takeover: bool) -> Result<()> {
    let mut export = selection::export(config, true)?;
    let base = Revision::load(export.tree.path(), config)?;
    let mut imported = import_shared(config, restic)?;
    if base.revision != imported.observed.head.revision {
        return Err(Error::new(
            "Stale authoring revision base; copy verified active_shared, omit generated registry-history.json, and reconcile edits before publishing; local work was preserved",
        ));
    }
    let candidate = Registry::load(export.tree.path(), config)?;
    Decisions::load(export.tree.path(), config)?;
    if !takeover
        && candidate.bots.iter().any(|entry| {
            imported
                .data
                .registry
                .bots
                .iter()
                .any(|old| old.id == entry.id && old.runtime_id != entry.runtime_id)
        })
    {
        return Err(Error::new(
            "Runtime change requires explicit --takeover acknowledgement; local work was preserved",
        ));
    }
    candidate.transition_from(&imported.data.registry, config, restic)?;
    imported
        .data
        .history
        .observe(&imported.data.registry, config, restic, false)?;
    imported
        .data
        .history
        .observe(&candidate, config, restic, true)?;
    imported.data.history.check(&candidate, restic)?;
    // The authoring input must omit this reserved generated file. Only the
    // accepted current snapshot supplies cumulative identifier history.
    imported.data.history.write(&mut export)?;
    let revision = imported
        .observed
        .head
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::new("Shared revision overflow"))?;
    Revision::new(config, revision).write(&mut export)?;
    budget::authorize(
        config,
        &[config
            .repositories
            .shared
            .url
            .as_deref()
            .expect("runtime configuration")],
        export.bytes,
    )?;
    let data = Prepared {
        registry: candidate,
        history: imported.data.history,
        export,
    };
    let store = HeadStore::new(config)?;
    if !commit_candidate(
        config,
        restic,
        &store,
        Some(&imported.observed),
        &data,
        BackupGuard::Persistent,
    )? {
        // A definite CAS loss is safe to reconcile with a fresh authoring base;
        // retain the losing immutable candidate for inspection.
        state::finish_backup(config.paths.state.as_ref().expect("runtime configuration"))?;
        return Err(Error::new(
            "Shared publication lost a head conflict; reconcile edits with the current authoring base and explicitly publish again; local work was preserved",
        ));
    }
    write_authoring(
        config,
        REVISION_FILE,
        &fs::read(data.export.tree.path().join(REVISION_FILE))?,
    )?;
    import_shared(config, restic)?.publish()?;
    cleanup_predecessors(
        config,
        restic,
        &config.repositories.shared,
        vec![imported.observed.head.snapshot],
    )
}

fn cleanup_predecessors(
    config: &Config,
    restic: &Restic<'_>,
    repository: &Repository,
    predecessors: Vec<String>,
) -> Result<()> {
    if !config.sync.versioning && !predecessors.is_empty() {
        budget::authorize(
            config,
            &[repository.url.as_deref().expect("runtime configuration")],
            0,
        )?;
        let mut args: Vec<std::ffi::OsString> = vec!["forget".into(), "--prune".into()];
        args.extend(predecessors.into_iter().map(std::ffi::OsString::from));
        restic.run(repository, &args, None)?;
    }
    Ok(())
}

fn destination(config: &Config, path: &Path) -> Result<std::path::PathBuf> {
    let path = absolute(&std::env::current_dir()?, path)?;
    if fs::symlink_metadata(&path).is_ok() {
        return Err(Error::new("Restore destination already exists"));
    }
    for root in [
        &config.paths.owned,
        &config.paths.shared,
        &config.paths.state,
    ]
    .into_iter()
    .flatten()
    {
        if path.starts_with(root) || root.starts_with(&path) {
            return Err(Error::new(
                "Restore destination must be separate from owned, shared, and state directories",
            ));
        }
    }
    if let Some(imports) = &config.paths.imports
        && imports.starts_with(&path)
    {
        return Err(Error::new(
            "Restore destination must not replace the imports root",
        ));
    }
    if config.storage.provider == "local" {
        for url in [
            &config.repositories.bot.url,
            &config.repositories.shared.url,
        ]
        .into_iter()
        .flatten()
        {
            if path.starts_with(url) || Path::new(url).starts_with(&path) {
                return Err(Error::new(
                    "Restore destination must be separate from repositories",
                ));
            }
        }
    }
    no_symlinks(&path)?;
    Ok(path)
}

fn recover(
    config: &Config,
    restic: &Restic<'_>,
    repository: &Repository,
    snapshot: &str,
    path: &Path,
) -> Result<()> {
    let path = destination(config, path)?;
    let snapshots = restic.snapshots(repository)?;
    let selected: Vec<_> = snapshots
        .iter()
        .filter(|s| s.id.starts_with(snapshot))
        .collect();
    if snapshot.len() < 8 || !snapshot.bytes().all(|c| c.is_ascii_hexdigit()) || selected.len() != 1
    {
        return Err(Error::new(
            "Snapshot ID must identify exactly one snapshot (at least eight hex digits)",
        ));
    }
    restore_selected(restic, repository, &selected[0].id, &path)
}

fn restore_selected(
    restic: &Restic<'_>,
    repository: &Repository,
    id: &str,
    path: &Path,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("Invalid restore destination"))?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".utsurigi-agent-hub-restore-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let tree = builder
        .tempdir_in(parent)
        .at("Cannot create restore staging in", parent)?;
    let staging = tree.path().to_owned();
    let result = restic
        .restore(repository, id, tree.path())
        .and_then(|()| fresh_destination(path, tree));
    if result.is_err() {
        let _ = selection::remove_tree(&staging);
    }
    result
}

fn pull(
    config: &Config,
    restic: &Restic<'_>,
    id: &str,
    path: &Path,
    allow_identity_mismatch: bool,
) -> Result<()> {
    let path = destination(config, path)?;
    let imported = import_shared(config, restic)?;
    imported.data.registry.access_checks(config)?;
    imported
        .data
        .history
        .check(&imported.data.registry, restic)?;
    let peer = imported.data.registry.entry(id)?;
    let repository = peer.repository();
    password(&repository)?;
    let snapshots = restic.snapshots(&repository)?;
    // Inspect the latest complete version; do not silently fall back to an old identity.
    let snapshot = snapshots
        .iter()
        .filter(|snapshot| snapshot.completed_at().is_some())
        .max_by_key(|snapshot| {
            (
                snapshot.completed_at(),
                DateTime::parse_from_rfc3339(&snapshot.time).ok(),
                &snapshot.id,
            )
        })
        .ok_or_else(|| Error::new("No complete peer snapshot is available"))?;
    if !snapshot.matches_identity(id, &peer.runtime_id, "bot") && !allow_identity_mismatch {
        return Err(Error::new(
            "Peer snapshot identity differs from the registry; inspect it and explicitly use --allow-identity-mismatch to continue",
        ));
    }
    restore_selected(restic, &repository, &snapshot.id, &path)
}

fn prune(config: &Config, restic: &Restic<'_>) -> Result<()> {
    let imported = import_shared(config, restic)?;
    imported.data.registry.access_checks(config)?;
    imported
        .data
        .history
        .check(&imported.data.registry, restic)?;
    // All shared controls, captures, accepted states and losing candidates are
    // retained in this version. Bot repository retention keeps its existing rule.
    let repository = &config.repositories.bot;
    password(repository)?;
    let snapshots: Vec<Snapshot> = restic
        .snapshots(repository)?
        .into_iter()
        .filter(|snapshot| snapshot.owned_by(config.id(), "bot"))
        .collect();
    let versions: Vec<_> = snapshots
        .iter()
        .map(|snapshot| retention::Version {
            id: snapshot.id.clone(),
            completed_at: snapshot.completed_at(),
        })
        .collect();
    let ids = retention::expired(
        &versions,
        Utc::now().timestamp(),
        config.sync.history_retention_seconds,
    );
    if !ids.is_empty() {
        budget::authorize(
            config,
            &[repository.url.as_deref().expect("runtime configuration")],
            0,
        )?;
        let mut args: Vec<std::ffi::OsString> = vec!["forget".into(), "--prune".into()];
        args.extend(ids.into_iter().map(std::ffi::OsString::from));
        restic.run(repository, &args, None)?;
    }
    Ok(())
}
