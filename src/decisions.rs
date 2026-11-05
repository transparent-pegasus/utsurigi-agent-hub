use crate::config::{Config, identity, no_symlinks, parse_yaml};
use crate::error::{Error, Result};
use crate::selection::Export;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const FILE: &str = "decisions.yaml";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Decisions {
    version: u32,
    hub: String,
    repository_sha256: String,
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

impl Decisions {
    fn parse(bytes: &[u8]) -> Result<Self> {
        let decisions: Self = parse_yaml(bytes, "Invalid decisions.yaml schema")?;
        if decisions.version != 1
            || !identity(&decisions.hub)
            || decisions.repository_sha256.len() != 64
            || !decisions
                .repository_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
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

    pub fn load(root: &Path, config: &Config) -> Result<Option<Self>> {
        let path = root.join(FILE);
        no_symlinks(&path)?;
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let decisions = Self::parse(&bytes)?;
        if decisions.hub != config.hub || decisions.repository_sha256 != repository_sha256(config) {
            return Err(Error::new(
                "Decision hub/repository scope does not match configuration",
            ));
        }
        Ok(Some(decisions))
    }
}

// Pure resolver: no configuration file, credentials, environment or storage.
// This record is one approved hub/repository decision, never a global default.
pub(crate) fn bundled_default(hub: &str, repository_sha256: &str) -> Result<Option<Decisions>> {
    let record = Decisions::parse(include_bytes!("default-decisions.yaml"))?;
    Ok((record.hub == hub && record.repository_sha256 == repository_sha256).then_some(record))
}

// Supplements apply only during migration and only when a remote record is
// absent. A complete remote record remains byte-for-byte unchanged.
pub fn supplement(export: &mut Export, config: &Config) -> Result<()> {
    let remote = Decisions::load(export.tree.path(), config)?;
    if remote
        .as_ref()
        .is_some_and(|record| record.caveman.is_some())
    {
        return Ok(());
    }
    let local_root = config.paths.shared.as_ref().expect("runtime configuration");
    let explicit = Decisions::load(local_root, config)?;
    let (mut local, bytes) = if let Some(local) = explicit.filter(|record| record.caveman.is_some())
    {
        (local, fs::read(local_root.join(FILE))?)
    } else if let Some(bundled) = bundled_default(&config.hub, &repository_sha256(config))? {
        (bundled, include_bytes!("default-decisions.yaml").to_vec())
    } else {
        return Ok(());
    };
    if let Some(mut remote) = remote {
        remote.caveman = local.caveman.take();
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
