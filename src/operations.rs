use crate::config::{Config, Repository, absolute, no_symlinks};
use crate::error::{Error, Result};
use crate::history::History;
use crate::registry::Registry;
use crate::restic::{Restic, Snapshot, latest};
use crate::selection;
use crate::shared::{self, SharedCache};
use crate::state::{Lock, State, fresh_destination, private_dir};
use crate::{Action, RegistryAction, RepositoryKind, budget};
use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use std::fs;
use std::path::Path;
use utsurigi_agent_hub::{retention, schedule};

struct Imported {
    registry: Registry,
    history: History,
    snapshot: String,
    cache: SharedCache,
}

impl Imported {
    fn publish(self) -> Result<()> {
        self.cache.publish(&self.snapshot)
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
        _ => (),
    }
    config.runtime()?;
    let root = config.paths.state.as_ref().expect("runtime configuration");
    let _lock = Lock::acquire(root)?;
    let mut state = State::load(root, &config.hub)?;
    if matches!(action, Action::Sync { scheduled: true })
        && state
            .next_due
            .is_some_and(|due| Utc::now().timestamp() < due)
    {
        println!("{}", serde_json::json!({"skipped": true}));
        return Ok(());
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
        Action::Setup => setup(&config, &restic, &mut state)?,
        Action::Sync { scheduled } => sync(&config, &restic, &mut state, scheduled)?,
        Action::Registry {
            command: RegistryAction::Publish { takeover },
        } => publish(&config, &restic, takeover)?,
        Action::Pull {
            bot_id,
            destination,
        } => pull(&config, &restic, &bot_id, &destination)?,
        Action::Restore {
            repository,
            snapshot,
            destination,
        } => {
            let repository = match repository {
                RepositoryKind::Shared => &config.repositories.shared,
                RepositoryKind::Bot => &config.repositories.bot,
            };
            recover(&config, &restic, repository, &snapshot, &destination)?;
        }
        Action::Prune => prune(&config, &restic)?,
        Action::Validate { .. } | Action::Status => unreachable!(),
    }
    println!("Operation completed");
    Ok(())
}

fn status_time(seconds: Option<i64>) -> Result<Option<String>> {
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
    let (next_action, remediation) = if !missing.is_empty() {
        (
            "configure",
            missing
                .iter()
                .map(|field| format!("Set {field} in configuration"))
                .collect::<Vec<_>>(),
        )
    } else if state.anchor.is_none() || state.next_due.is_none() {
        (
            "setup",
            vec!["Run setup to initialize shared files and the schedule".to_owned()],
        )
    } else if active_shared.is_none() {
        (
            "sync",
            vec!["Run sync to migrate the initialized schedule to the shared cache".to_owned()],
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
            "hub": config.hub,
            "ready": missing.is_empty(), "last_success": status_time(state.last_success)?,
            "anchor": status_time(state.anchor)?, "next_due": status_time(state.next_due)?, "active_shared": active_shared,
            "next_action": next_action, "remediation": remediation,
        })
    );
    Ok(())
}

fn import_shared(config: &Config, restic: &Restic<'_>) -> Result<Imported> {
    let cache = SharedCache::open(config)?;
    let snapshots = restic.snapshots(&config.repositories.shared)?;
    let snapshot = latest(&snapshots, "shared").ok_or_else(|| {
        Error::new("No complete shared registry exists; setup by its owner is required")
    })?;
    let (registry, history) = cache.load(
        &snapshot.id,
        |path| restic.restore(&config.repositories.shared, &snapshot.id, path),
        |path| {
            selection::validate_import(path)?;
            let registry = Registry::load(path, config)?;
            if !snapshot.owned_by(&registry.owner.bot_id, "shared")
                || !snapshot
                    .tags
                    .contains(&format!("runtime:{}", registry.owner.runtime_id))
            {
                return Err(Error::new(
                    "Shared snapshot ownership does not match its registry",
                ));
            }
            let history = History::load(path, &registry)?;
            Ok((registry, history))
        },
    )?;
    Ok(Imported {
        registry,
        history,
        snapshot: snapshot.id.clone(),
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
        if restic.repository_id(&config.repositories.bot)?
            == restic.repository_id(&config.repositories.shared)?
        {
            return Err(Error::new(
                "Private Bot data requires a separate repository and master key",
            ));
        }
    }
    Ok(())
}

fn setup(config: &Config, restic: &Restic<'_>, state: &mut State) -> Result<()> {
    // Reject unsafe owned selections before publishing the initial shared tree.
    drop(selection::export(config, false)?);
    let snapshots = match restic.snapshots(&config.repositories.shared) {
        Ok(snapshots) => snapshots,
        Err(error) if error.code == 10 => Vec::new(),
        Err(error) => return Err(error),
    };
    let imported = if latest(&snapshots, "shared").is_none() {
        let registry = Registry::load(
            config.paths.shared.as_ref().expect("runtime configuration"),
            config,
        )?;
        if !registry.is_owner(config) {
            return Err(Error::new(
                "Only the initial registry owner can initialize shared storage",
            ));
        }
        registry.current(config)?;
        registry.credentials(config)?;
        if snapshots.iter().any(|snapshot| {
            !snapshot.owned_by(config.id(), "shared")
                || !snapshot
                    .tags
                    .iter()
                    .filter_map(|tag| tag.strip_prefix("bot:"))
                    .eq([config.id()])
                || !snapshot
                    .tags
                    .iter()
                    .filter_map(|tag| tag.strip_prefix("runtime:"))
                    .eq([config.runtime_id()])
                || !snapshot
                    .tags
                    .iter()
                    .filter_map(|tag| tag.strip_prefix("kind:"))
                    .eq(["shared"])
                || snapshot
                    .tags
                    .iter()
                    .any(|tag| tag.starts_with("completed:"))
        }) {
            return Err(Error::new(
                "Shared repository has uncertain history; explicit recovery is required",
            ));
        }
        // Reject a Bot repository from another hub before initializing either repository.
        match restic.snapshots(&config.repositories.bot) {
            Ok(_) => (),
            Err(error) if error.code == 10 => (),
            Err(error) => return Err(error),
        }
        let mut history = History::new();
        history.observe(&registry, config, restic, true)?;
        history.check(&registry, restic)?;
        let mut export = selection::export(config, true)?;
        history.write(&mut export)?;
        let repositories = &[
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
        ];
        budget::authorize(config, repositories, export.bytes)?;
        restic.initialize(&config.repositories.shared)?;
        restic.initialize(&config.repositories.bot)?;
        history.observe(&registry, config, restic, true)?;
        private_repository(config, restic, &history)?;
        // Initialization can add master IDs; include their bytes before authorizing backup.
        drop(export);
        let mut export = selection::export(config, true)?;
        history.write(&mut export)?;
        budget::authorize(config, repositories, export.bytes)?;
        restic.backup(&config.repositories.shared, &export, "shared")?;
        let imported = import_shared(config, restic)?;
        imported.registry.current(config)?;
        imported.registry.credentials(config)?;
        imported.history.check(&imported.registry, restic)?;
        private_repository(config, restic, &imported.history)?;
        imported
    } else {
        let mut imported = import_shared(config, restic)?;
        imported.registry.current_runtime(config)?;
        if imported.registry.current(config).is_err() {
            if !imported.registry.is_owner(config) {
                return Err(Error::new(
                    "Joining Bots may initialize only their registered repository",
                ));
            }
            let candidate = Registry::load(
                config.paths.shared.as_ref().expect("runtime configuration"),
                config,
            )?;
            if !candidate.is_owner(config) {
                return Err(Error::new(
                    "Repository changes require the current owner's local registry",
                ));
            }
            candidate.current(config)?;
            candidate.transition_from(&imported.registry, config, restic)?;
            imported
                .history
                .observe(&imported.registry, config, restic, false)?;
            imported.history.observe(&candidate, config, restic, true)?;
            imported.history.check(&candidate, restic)?;
        } else {
            imported.registry.credentials(config)?;
            imported.history.check(&imported.registry, restic)?;
        }
        budget::authorize(
            config,
            &[config
                .repositories
                .bot
                .url
                .as_deref()
                .expect("runtime configuration")],
            0,
        )?;
        restic.initialize(&config.repositories.bot)?;
        private_repository(config, restic, &imported.history)?;
        imported
    };
    initialize_schedule(config, state)?;
    imported.publish()?;
    state.save(config.paths.state.as_ref().expect("runtime configuration"))
}

fn sync(config: &Config, restic: &Restic<'_>, state: &mut State, scheduled: bool) -> Result<()> {
    let anchor = state
        .anchor
        .ok_or_else(|| Error::new("Run setup to initialize the schedule"))?;
    if state.next_due.is_none() {
        return Err(Error::new("Run setup to initialize the schedule"));
    }
    let imported = import_shared(config, restic)?;
    imported.registry.current(config)?;
    imported.registry.credentials(config)?;
    private_repository(config, restic, &imported.history)?;
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
    restic.backup(&config.repositories.bot, &export, "bot")?;
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
    state.save(config.paths.state.as_ref().expect("runtime configuration"))
}

fn publish(config: &Config, restic: &Restic<'_>, takeover: bool) -> Result<()> {
    let candidate = Registry::load(
        config.paths.shared.as_ref().expect("runtime configuration"),
        config,
    )?;
    if !candidate.is_owner(config) {
        return Err(Error::new(
            "Only the candidate registry owner can publish shared knowledge",
        ));
    }
    candidate.current(config)?;
    let mut imported = import_shared(config, restic)?;
    if !imported.registry.is_owner(config) && !takeover {
        return Err(Error::new(
            "Current ownership differs; explicit user-directed --takeover is required after stopping the prior runtime",
        ));
    }
    candidate.transition_from(&imported.registry, config, restic)?;
    imported
        .history
        .observe(&imported.registry, config, restic, false)?;
    imported.history.observe(&candidate, config, restic, true)?;
    imported.history.check(&candidate, restic)?;
    private_repository(config, restic, &imported.history)?;
    let mut export = selection::export(config, true)?;
    imported.history.write(&mut export)?;
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
    restic.backup(&config.repositories.shared, &export, "shared")?;
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
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("Invalid restore destination"))?;
    let tree = tempfile::Builder::new()
        .prefix(".utsurigi-agent-hub-restore-")
        .tempdir_in(parent)?;
    restic.restore(repository, &selected[0].id, tree.path())?;
    fresh_destination(&path, tree)
}

fn pull(config: &Config, restic: &Restic<'_>, id: &str, path: &Path) -> Result<()> {
    destination(config, path)?;
    let imported = import_shared(config, restic)?;
    imported.registry.current(config)?;
    imported.registry.credentials(config)?;
    let peer = imported.registry.entry(id)?;
    if !peer.shared {
        return Err(Error::new(
            "Peer sharing is disabled; separate repository and password access is required",
        ));
    }
    let repository = peer.repository();
    let snapshots: Vec<_> = restic
        .snapshots(&repository)?
        .into_iter()
        .filter(|s| s.owned_by(id, "bot"))
        .collect();
    let snapshot = latest(&snapshots, "bot")
        .ok_or_else(|| Error::new("No complete peer snapshot is available"))?;
    recover(config, restic, &repository, &snapshot.id, path)
}

fn prune(config: &Config, restic: &Restic<'_>) -> Result<()> {
    let imported = import_shared(config, restic)?;
    imported.registry.current(config)?;
    imported.registry.credentials(config)?;
    let mut repositories = vec![(&config.repositories.bot, "bot")];
    if imported.registry.is_owner(config) {
        repositories.push((&config.repositories.shared, "shared"));
    }
    for (repository, kind) in repositories {
        let snapshots: Vec<Snapshot> = restic
            .snapshots(repository)?
            .into_iter()
            .filter(|s| s.owned_by(config.id(), kind))
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
    }
    Ok(())
}
