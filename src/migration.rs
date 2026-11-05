//! Automatic v1 cutover. The outer stdin-command backup owns the native append
//! lock from before source selection until the incompatible fence is durable.
//! Captures are ordinary retained snapshots, so source retention does not depend
//! on an application ID being treated as a reference by restic's GC.

use crate::budget;
use crate::config::{Config, identity};
use crate::error::{Error, Result};
use crate::history::History;
use crate::registry::{LegacyRegistry, Registry};
use crate::restic::{Restic, Snapshot, hex_digest, latest, password};
use crate::selection::{self, Export};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const HELPER: &str = "__uah-migration-fence";
pub(crate) const FENCE_FILE: &str = "format-fence.json";
const FENCE_TYPE: &str = "utsurigi-agent-hub/format-fence";
const CAPTURE_KIND: &str = "migration-capture";
const MAX_FENCE_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub hub: String,
    pub source: Option<String>,
    pub capture: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    format: u32,
    hub: String,
    #[serde(deserialize_with = "required_option")]
    source: Option<String>,
    #[serde(deserialize_with = "required_option")]
    capture: Option<String>,
}

fn required_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::deserialize(deserializer)
}

impl From<Fence> for Payload {
    fn from(fence: Fence) -> Self {
        Self {
            kind: FENCE_TYPE.into(),
            format: 2,
            hub: fence.hub,
            source: fence.source,
            capture: fence.capture,
        }
    }
}

pub(crate) fn control_tags(hub: &str) -> [String; 3] {
    [
        "uah2:kind:format-fence".into(),
        "uah2:format:2".into(),
        format!("uah2:hub:{hub}"),
    ]
}

fn temporary(config: &Config, prefix: &str) -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(config.paths.state.as_ref().expect("runtime configuration"))?)
}

/// Recognize only this exact dedicated control, and only via the shared-repo
/// caller in Restic::snapshots. A foreign/mixed/unknown control is never ignored.
pub(crate) fn read_control(
    config: &Config,
    restic: &Restic<'_>,
    snapshot: &Snapshot,
) -> Result<Fence> {
    let invalid = || Error::new("Invalid or incompatible migration format fence");
    let tags = control_tags(&config.hub);
    if !hex_digest(&snapshot.id)
        || snapshot.tags.len() != tags.len()
        || tags.iter().any(|tag| !snapshot.tags.contains(tag))
    {
        return Err(invalid());
    }
    let tree = temporary(config, "fence-read-")?;
    restic.restore(&config.repositories.shared, &snapshot.id, tree.path())?;
    let path = tree.path().join(FENCE_FILE);
    if fs::read_dir(tree.path())?.count() != 1
        || !fs::metadata(&path).map_err(|_| invalid())?.is_file()
        || fs::metadata(&path)?.len() > MAX_FENCE_BYTES
    {
        return Err(invalid());
    }
    let payload: Payload = serde_json::from_slice(&fs::read(path)?).map_err(|_| invalid())?;
    if payload.kind != FENCE_TYPE || payload.format != 2 || payload.hub != config.hub {
        return Err(invalid());
    }
    match (&payload.source, &payload.capture) {
        (Some(source), Some(capture))
            if hex_digest(source)
                && hex_digest(capture)
                && !source.eq_ignore_ascii_case(capture)
                && !source.eq_ignore_ascii_case(&snapshot.id)
                && !capture.eq_ignore_ascii_case(&snapshot.id) => {}
        (None, None) => (),
        _ => return Err(invalid()),
    }
    Ok(Fence {
        hub: payload.hub,
        source: payload.source,
        capture: payload.capture,
    })
}

fn one_tag<'a>(snapshot: &'a Snapshot, prefix: &str) -> Option<&'a str> {
    let mut tags = snapshot
        .tags
        .iter()
        .filter_map(|tag| tag.strip_prefix(prefix));
    let value = tags.next()?;
    tags.next().is_none().then_some(value)
}

// This is the sole v1-reader adapter. The operations integration can substitute
// its separate strict v1 type here, keeping all source-tag/history/cap checks.
// `legacy` is present for original-source validation under the append lock;
// captures are already pinned by a validated fence and have a distinct kind.
fn load_legacy_source(
    root: &Path,
    config: &Config,
    legacy: Option<&Snapshot>,
) -> Result<(Registry, History)> {
    selection::validate_import(root)?;
    let legacy_registry = LegacyRegistry::load(root, config)?;
    let registry = legacy_registry.convert(config)?;
    if let Some(snapshot) = legacy
        && (!snapshot.owned_by(&legacy_registry.owner.bot_id, "shared")
            || one_tag(snapshot, "hub:") != Some(config.hub.as_str())
            || one_tag(snapshot, "bot:") != Some(legacy_registry.owner.bot_id.as_str())
            || one_tag(snapshot, "runtime:") != Some(legacy_registry.owner.runtime_id.as_str())
            || one_tag(snapshot, "kind:") != Some("shared")
            || snapshot.completed_at().is_none()
            || chrono::DateTime::parse_from_rfc3339(&snapshot.time).is_err())
    {
        return Err(Error::new(
            "Legacy shared snapshot identity does not match its registry",
        ));
    }
    let history = History::load(root, &registry)?;
    let own = registry.bots.iter().find(|entry| entry.id == config.id());
    if own.is_some() {
        registry.current(config)?;
    } else if registry
        .bots
        .iter()
        .any(|entry| Some(&entry.repository) == config.repositories.bot.url.as_ref())
    {
        return Err(Error::new(
            "Joining Bot repository conflicts with the legacy registry",
        ));
    }
    let needed = registry
        .bots
        .len()
        .checked_add(usize::from(own.is_none()))
        .ok_or_else(|| Error::new("Invalid migration fleet size"))?;
    if needed > config.budget.max_bots {
        return Err(Error::new(
            "Migration max_bots cannot fit the existing roster and joining Bot",
        ));
    }
    Ok((registry, history))
}

fn repositories(config: &Config) -> [&str; 2] {
    [
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
    ]
}

fn source_checks(
    config: &Config,
    restic: &Restic<'_>,
    registry: &Registry,
    history: &History,
) -> Result<()> {
    history.check(registry, restic)?;
    if !config.sharing.bot_data {
        history.check_private(&config.repositories.bot, restic)?;
    }
    Ok(())
}

// An interrupted first v1 backup is recovery data, not an accepted source.
// Only the same initial Bot/runtime may retry from its validated local candidate.
fn validate_initial_artifacts(config: &Config, snapshots: &[Snapshot]) -> Result<()> {
    if snapshots.iter().any(|snapshot| {
        !snapshot.owned_by(config.id(), "shared")
            || one_tag(snapshot, "hub:") != Some(config.hub.as_str())
            || one_tag(snapshot, "bot:") != Some(config.id())
            || one_tag(snapshot, "runtime:") != Some(config.runtime_id())
            || one_tag(snapshot, "kind:") != Some("shared")
            || snapshot
                .tags
                .iter()
                .any(|tag| tag.starts_with("completed:"))
    }) {
        return Err(Error::new(
            "Shared repository has uncertain history; initial retry is unsafe",
        ));
    }
    Ok(())
}

fn initial_inventory(snapshots: &[Snapshot]) -> String {
    let mut ids: Vec<_> = snapshots.iter().map(|snapshot| &snapshot.id).collect();
    ids.sort();
    let mut digest = Sha256::new();
    for id in ids {
        digest.update(id.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

fn parent_prerequisites(config: &Config, restic: &Restic<'_>) -> Result<()> {
    config.runtime()?;
    let shared = password(&config.repositories.shared)?;
    let own = password(&config.repositories.bot)?;
    if !config.sharing.bot_data
        && (shared == own
            || config.repositories.shared.password_env == config.repositories.bot.password_env
            || config
                .repositories
                .shared
                .password_ref
                .as_ref()
                .filter(|value| !value.is_empty())
                .is_some_and(|value| Some(value) == config.repositories.bot.password_ref.as_ref()))
    {
        return Err(Error::new(
            "Private Bot data requires separate password access",
        ));
    }
    let export = selection::export(config, false)?;
    budget::authorize(config, &[repositories(config)[1]], export.bytes)?;
    match restic.snapshots(&config.repositories.bot) {
        Ok(_) => {
            if !config.sharing.bot_data {
                let own_id = restic.repository_id(&config.repositories.bot)?;
                match restic.repository_id(&config.repositories.shared) {
                    Ok(id) if id == own_id => {
                        return Err(Error::new(
                            "Private Bot data requires a separate repository and master key",
                        ));
                    }
                    Ok(_) => (),
                    Err(error) if error.code == 10 => (),
                    Err(error) => return Err(error),
                }
            }
        }
        Err(error) if error.code == 10 => (),
        Err(error) => return Err(error),
    }
    Ok(())
}

fn existing_fence(
    config: &Config,
    restic: &Restic<'_>,
    snapshots: &[Snapshot],
    parent: bool,
) -> Result<Option<Fence>> {
    let mut controls: Vec<_> = snapshots
        .iter()
        .filter(|snapshot| snapshot.tags.iter().any(|tag| tag.starts_with("uah2:")))
        .collect();
    controls.sort_by(|left, right| left.id.cmp(&right.id));
    let mut selected: Option<(Fence, Option<Manifest>)> = None;
    for snapshot in controls {
        let fence = read_control(config, restic, snapshot)?;
        let manifest = validate_capture(config, restic, &fence, parent)?;
        if selected.as_ref().is_some_and(|(old, old_manifest)| {
            old.source != fence.source || old_manifest != &manifest
        }) {
            return Err(Error::new(
                "Contradictory migration format fences; recovery is required",
            ));
        }
        if selected.is_none() {
            selected = Some((fence, manifest));
        }
    }
    Ok(selected.map(|(fence, _)| fence))
}

fn validate_capture(
    config: &Config,
    restic: &Restic<'_>,
    fence: &Fence,
    parent: bool,
) -> Result<Option<Manifest>> {
    let Some(id) = &fence.capture else {
        return Ok(None);
    };
    let snapshot = restic.snapshot(&config.repositories.shared, id)?;
    if !snapshot.kind(CAPTURE_KIND)
        || snapshot.tags.len() != 6
        || one_tag(&snapshot, "hub:") != Some(config.hub.as_str())
        || one_tag(&snapshot, "kind:") != Some(CAPTURE_KIND)
        || one_tag(&snapshot, "bot:") != Some(snapshot.hostname.as_str())
        || !identity(&snapshot.hostname)
        || !one_tag(&snapshot, "runtime:").is_some_and(identity)
        || !one_tag(&snapshot, "run:").is_some_and(identity)
    {
        return Err(Error::new("Invalid retained migration capture metadata"));
    }
    let tree = temporary(config, "capture-read-")?;
    restic.restore(&config.repositories.shared, id, tree.path())?;
    let (registry, history) = load_legacy_source(tree.path(), config, None)?;
    if parent {
        registry.credentials(config)?;
    }
    source_checks(config, restic, &registry, &history)?;
    let (manifest, bytes) = tree_manifest(tree.path())?;
    budget::authorize(config, &repositories(config), bytes)?;
    Ok(Some(manifest))
}

/// Return the durable migration input. Activation/conversion belongs to the
/// operations track, which must adopt only a successful conditional head write.
/// `preflight` MUST build/validate/budget the exact final v2 registration export
/// before this layer writes an incompatible fence. It receives the verified v1
/// source, or None for an empty hub. On durable-fence resume it receives only the
/// verified capture (or None); a late legacy snapshot is never selected.
/// The supplied path is valid for the duration of the callback.
pub fn ensure(
    config: &Config,
    restic: &Restic<'_>,
    preflight: &mut dyn FnMut(Option<&Path>) -> Result<()>,
) -> Result<Fence> {
    parent_prerequisites(config, restic)?;
    let snapshots = match restic.snapshots(&config.repositories.shared) {
        Ok(snapshots) => snapshots,
        Err(error) if error.code == 10 => {
            budget::authorize(config, &repositories(config), 0)?;
            restic.initialize(&config.repositories.shared)?;
            restic.snapshots(&config.repositories.shared)?
        }
        Err(error) => return Err(error),
    };
    if let Some(fence) = existing_fence(config, restic, &snapshots, true)? {
        if let Some(capture) = &fence.capture {
            let tree = temporary(config, "migration-resume-preflight-")?;
            restic.restore(&config.repositories.shared, capture, tree.path())?;
            preflight(Some(tree.path()))?;
        } else {
            preflight(None)?;
        }
        return Ok(fence);
    }
    let source = if let Some(source) = latest(&snapshots, "shared") {
        let tree = temporary(config, "migration-preflight-")?;
        restic.restore(&config.repositories.shared, &source.id, tree.path())?;
        let (registry, history) = load_legacy_source(tree.path(), config, Some(source))?;
        registry.credentials(config)?;
        source_checks(config, restic, &registry, &history)?;
        budget::authorize(config, &repositories(config), tree_manifest(tree.path())?.1)?;
        preflight(Some(tree.path()))?;
        source.id.clone()
    } else {
        validate_initial_artifacts(config, &snapshots)?;
        preflight(None)?;
        if snapshots.is_empty() {
            "empty".to_owned()
        } else {
            format!("incomplete:{}", initial_inventory(&snapshots))
        }
    };
    let config_path = config
        .source_path
        .as_deref()
        .ok_or_else(|| Error::new("Migration requires a canonical configuration source path"))?;
    let config_digest = config
        .source_digest
        .as_deref()
        .ok_or_else(|| Error::new("Migration requires the parsed configuration digest"))?;
    let id = restic.format_fence(config_path, &source, config_digest)?;
    // Read back the exact immutable control, then check the whole inventory for
    // contradictory concurrent fences. Never select a later legacy snapshot.
    let snapshot = restic.snapshot(&config.repositories.shared, &id)?;
    read_control(config, restic, &snapshot)?;
    let snapshots = restic.snapshots(&config.repositories.shared)?;
    existing_fence(config, restic, &snapshots, true)?
        .ok_or_else(|| Error::new("Migration fence could not be reconciled; no write was replayed"))
}

pub(crate) fn helper_command() -> Option<Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(HELPER)) {
        return None;
    }
    Some((|| {
        let path = args
            .next()
            .ok_or_else(|| Error::new("Migration helper requires a configuration path"))?;
        let expected_source = args
            .next()
            .and_then(|value| value.into_string().ok())
            .filter(|value| {
                value == "empty"
                    || hex_digest(value)
                    || value.strip_prefix("incomplete:").is_some_and(hex_digest)
            })
            .ok_or_else(|| {
                Error::new("Migration helper requires a full source ID or initial inventory")
            })?;
        let expected_config_digest = args
            .next()
            .and_then(|value| value.into_string().ok())
            .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| Error::new("Migration helper requires a configuration digest"))?;
        if args.next().is_some() {
            return Err(Error::new("Unexpected migration helper arguments"));
        }
        let config = Config::load(Path::new(&path))?;
        if config.source_path.as_deref() != Some(Path::new(&path))
            || config.source_digest.as_deref() != Some(expected_config_digest.as_str())
        {
            return Err(Error::new(
                "Configuration changed before migration; retry setup",
            ));
        }
        config.runtime()?;
        let shared_password = std::env::var_os("RESTIC_PASSWORD")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::new("Migration helper requires inherited RESTIC_PASSWORD"))?;
        // This synchronous hidden entry point has not started any threads.
        // Keep this credential solely on Restic's shared-repository path: a
        // peer whose configured env is RESTIC_PASSWORD must not receive it.
        unsafe { std::env::remove_var("RESTIC_PASSWORD") };
        let mut restic = Restic::open(&config, false)?;
        restic.inherit_shared_password(shared_password);
        let fence = capture_under_append_lock(&config, &restic, &expected_source)?;
        let bytes = serde_json::to_vec(&Payload::from(fence))
            .map_err(|_| Error::new("Cannot encode migration fence"))?;
        budget::authorize(&config, &[repositories(&config)[0]], bytes.len() as u64)?;
        std::io::stdout().lock().write_all(&bytes)?;
        Ok(())
    })())
}

fn capture_under_append_lock(
    config: &Config,
    restic: &Restic<'_>,
    expected: &str,
) -> Result<Fence> {
    let expected_source = hex_digest(expected).then_some(expected);
    let snapshots = restic.snapshots(&config.repositories.shared)?;
    if let Some(fence) = existing_fence(config, restic, &snapshots, false)? {
        if fence.source.as_deref() != expected_source {
            return Err(Error::new(
                "Migration source changed before the append lock; retry setup",
            ));
        }
        return Ok(fence);
    }
    let Some(expected_source) = expected_source else {
        validate_initial_artifacts(config, &snapshots)?;
        let unchanged = if expected == "empty" {
            snapshots.is_empty()
        } else {
            expected.strip_prefix("incomplete:") == Some(initial_inventory(&snapshots).as_str())
        };
        if !unchanged {
            return Err(Error::new(
                "Shared repository changed before initial migration; retry setup",
            ));
        }
        return Ok(Fence {
            hub: config.hub.clone(),
            source: None,
            capture: None,
        });
    };
    let source = latest(&snapshots, "shared")
        .ok_or_else(|| Error::new("No complete legacy migration source under the append lock"))?;
    if !source.id.eq_ignore_ascii_case(expected_source) {
        return Err(Error::new(
            "Legacy source changed before migration; retry setup",
        ));
    }
    let tree = temporary(config, "migration-capture-")?;
    restic.restore(&config.repositories.shared, &source.id, tree.path())?;
    let (registry, history) = load_legacy_source(tree.path(), config, Some(source))?;
    source_checks(config, restic, &registry, &history)?;
    let (manifest, bytes) = tree_manifest(tree.path())?;
    budget::authorize(config, &repositories(config), bytes)?;
    let export = Export { tree, bytes };
    let capture = restic.backup_uncommitted(
        &config.repositories.shared,
        &export,
        CAPTURE_KIND,
        crate::restic::BackupGuard::InitialSetup,
    )?;
    let verified = temporary(config, "capture-verify-")?;
    restic.restore(&config.repositories.shared, &capture, verified.path())?;
    if tree_manifest(verified.path())?.0 != manifest {
        return Err(Error::new(
            "Retained migration capture does not preserve the complete source tree",
        ));
    }
    Ok(Fence {
        hub: config.hub.clone(),
        source: Some(source.id.clone()),
        capture: Some(capture),
    })
}

// Include directories and every regular file. Re-export filtering would lose
// source bytes if exclusions changed, and an ID-only pin would not prevent GC.
type Manifest = BTreeMap<PathBuf, Option<(u64, [u8; 32])>>;

fn tree_manifest(root: &Path) -> Result<(Manifest, u64)> {
    fn visit(root: &Path, path: &Path, manifest: &mut Manifest, bytes: &mut u64) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .map_err(|_| Error::new("Invalid restored source path"))?
                .to_owned();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                manifest.insert(name, None);
                visit(root, &path, manifest, bytes)?;
            } else if kind.is_file() {
                let mut file = File::open(&path)?;
                let mut digest = Sha256::new();
                let mut size = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    digest.update(&buffer[..count]);
                    size = size
                        .checked_add(count as u64)
                        .ok_or_else(|| Error::new("Source size overflow"))?;
                }
                *bytes = bytes
                    .checked_add(size)
                    .ok_or_else(|| Error::new("Source size overflow"))?;
                manifest.insert(name, Some((size, digest.finalize().into())));
            } else {
                return Err(Error::new(
                    "Migration source contains symlinks or special files",
                ));
            }
        }
        Ok(())
    }
    let mut manifest = BTreeMap::new();
    let mut bytes = 0;
    visit(root, root, &mut manifest, &mut bytes)?;
    Ok((manifest, bytes))
}
