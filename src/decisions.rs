use crate::config::{Config, identity, no_symlinks, parse_yaml};
use crate::error::{Error, Result};
use crate::selection::Export;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub(crate) const FILE: &str = "decisions.yaml";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decisions {
    version: u32,
    hub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caveman: Option<Caveman>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Caveman {
    enabled: bool,
    basis: Basis,
    source: String,
    recorded_by: String,
    recorded_at: String,
    original_decision_at: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Basis {
    UserDecision,
    DerivedDefault,
}

pub fn repository_sha256(config: &Config) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            config
                .repositories
                .shared
                .url
                .as_deref()
                .expect("runtime configuration")
                .as_bytes()
        )
    )
}

/// The shared repository a record is scoped to. Current records carry its restic
/// repository ID; legacy records carry the SHA-256 of the configured shared URL,
/// which changes when the repository moves.
#[derive(Clone, Copy)]
pub(crate) enum Scope<'a> {
    Id(&'a str),
    Legacy(&'a str),
}

impl<'a> Scope<'a> {
    /// `current` is the record version that carries `repository_id`; the one before it is legacy.
    pub(crate) fn of(
        version: u32,
        current: u32,
        id: Option<&'a str>,
        digest: Option<&'a str>,
    ) -> Option<Self> {
        let scope = match (id, digest) {
            (Some(id), None) if version == current => Self::Id(id),
            (None, Some(digest)) if version == current - 1 => Self::Legacy(digest),
            _ => return None,
        };
        let (Self::Id(value) | Self::Legacy(value)) = scope;
        (value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
        .then_some(scope)
    }
}

/// Where a record was read, which decides how its repository scope is checked.
#[derive(Clone, Copy)]
pub(crate) enum Source<'a> {
    /// Restored from the configured shared repository and matched to its accepted head.
    Imported,
    /// Local input. `id` is the configured shared repository's restic ID, None until the
    /// repository exists; `head` is the same file in the accepted head snapshot.
    Local {
        id: Option<&'a str>,
        head: Option<&'a [u8]>,
    },
}

impl Source<'_> {
    /// `digest` is the SHA-256 of the configured shared URL; `bytes` is the record's file.
    pub(crate) fn accepts(self, scope: Scope<'_>, digest: &str, bytes: &[u8]) -> bool {
        match (self, scope) {
            (Self::Imported, _) => true,
            (Self::Local { id, .. }, Scope::Id(value)) => id == Some(value),
            // An unmoved URL, or after relocation the accepted head's exact legacy file.
            (Self::Local { head, .. }, Scope::Legacy(value)) => {
                value == digest || head == Some(bytes)
            }
        }
    }
}

impl Decisions {
    fn scope(&self) -> Option<Scope<'_>> {
        Scope::of(
            self.version,
            2,
            self.repository_id.as_deref(),
            self.repository_sha256.as_deref(),
        )
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        let decisions: Self = parse_yaml(bytes, "Invalid decisions.yaml schema")?;
        if !identity(&decisions.hub) || decisions.scope().is_none() {
            return Err(Error::new("Invalid decisions.yaml version or scope"));
        }
        if let Some(record) = &decisions.caveman
            && (record.source.trim().is_empty()
                || record.recorded_by.trim().is_empty()
                || chrono::DateTime::parse_from_rfc3339(&record.recorded_at).is_err()
                || record
                    .original_decision_at
                    .as_ref()
                    .is_some_and(|time| chrono::DateTime::parse_from_rfc3339(time).is_err()))
        {
            return Err(Error::new("Invalid decision provenance or timestamp"));
        }
        Ok(decisions)
    }

    pub fn load(root: &Path, config: &Config, source: Source<'_>) -> Result<Option<Self>> {
        let path = root.join(FILE);
        no_symlinks(&path)?;
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let decisions = Self::parse(&bytes)?;
        let mismatch = || Error::new("Decision hub/repository scope does not match configuration");
        if decisions.hub != config.hub {
            return Err(mismatch());
        }
        let scope = decisions.scope().ok_or_else(mismatch)?;
        if matches!(
            (source, scope),
            (Source::Local { id: None, .. }, Scope::Id(_))
        ) {
            return Err(Error::new(
                "decisions.yaml version 2 requires an existing shared repository; for a new hub write version 1, or copy repository_id from active_shared after setup",
            ));
        }
        if !source.accepts(scope, &repository_sha256(config), &bytes) {
            return Err(mismatch());
        }
        Ok(Some(decisions))
    }
}

// Pure resolver: no configuration file, credentials, environment or storage.
// This record is one approved hub/repository decision, never a global default.
pub(crate) fn bundled_default(
    hub: &str,
    repository_sha256: &str,
    repository_id: &str,
) -> Result<Option<Decisions>> {
    let bytes = include_bytes!("default-decisions.yaml");
    let record = Decisions::parse(bytes)?;
    let source = Source::Local {
        id: Some(repository_id),
        head: None,
    };
    let scoped = record.hub == hub
        && record
            .scope()
            .is_some_and(|scope| source.accepts(scope, repository_sha256, bytes));
    Ok(scoped.then_some(record))
}

// Supplements apply only during migration and only when a remote record is
// absent. A complete remote record remains byte-for-byte unchanged.
// `id` is the configured shared repository's restic ID.
pub fn supplement(export: &mut Export, config: &Config, id: &str) -> Result<()> {
    let source = Source::Local {
        id: Some(id),
        head: None,
    };
    let remote = Decisions::load(export.tree.path(), config, source)?;
    if remote
        .as_ref()
        .is_some_and(|record| record.caveman.is_some())
    {
        return Ok(());
    }
    let local_root = config.paths.shared.as_ref().expect("runtime configuration");
    let explicit = Decisions::load(local_root, config, source)?;
    let (mut local, bytes) = if let Some(local) = explicit.filter(|record| record.caveman.is_some())
    {
        (local, fs::read(local_root.join(FILE))?)
    } else if let Some(bundled) = bundled_default(&config.hub, &repository_sha256(config), id)? {
        (bundled, include_bytes!("default-decisions.yaml").to_vec())
    } else {
        return Ok(());
    };
    if let Some(mut remote) = remote {
        remote.caveman = local.caveman.take();
        // The CLI encodes this merged record itself, so it takes the current format.
        remote.version = 2;
        remote.repository_sha256 = None;
        remote.repository_id = Some(id.to_owned());
        let bytes = serde_saphyr::to_string(&remote)
            .map_err(|_| Error::new("Cannot encode decisions.yaml"))?;
        export.write(FILE, bytes.as_bytes())
    } else {
        export.write(FILE, &bytes)
    }
}

#[cfg(test)]
#[path = "decisions_tests.rs"]
mod tests;
