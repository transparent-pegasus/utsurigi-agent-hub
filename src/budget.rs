use crate::config::{Config, no_symlinks};
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attestation {
    expires_at: DateTime<chrono::FixedOffset>,
    repositories: Vec<String>,
    max_source_bytes: u64,
}

pub fn authorize(config: &Config, repositories: &[&str], source_bytes: u64) -> Result<()> {
    if config.storage.provider == "local" || !config.budget.free_tier_only {
        return Ok(());
    }
    let held = || {
        Error::new(
            "Budget hold: host-issued, private, unexpired attestation required for this repository and source size; host verifies billing, quota, and encryption overhead",
        )
    };
    let path = config.budget.attestation_file.as_ref().ok_or_else(held)?;
    no_symlinks(path).map_err(|_| held())?;
    let metadata = fs::metadata(path).map_err(|error| {
        Error::new(format!(
            "Budget hold: cannot inspect {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(held());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(held());
        }
    }
    #[cfg(not(unix))]
    return Err(Error::new(
        "Private attestation permissions require a supported Unix host",
    ));
    let attestation: Attestation = serde_json::from_slice(&fs::read(path).map_err(|error| {
        Error::new(format!(
            "Budget hold: cannot read {}: {error}",
            path.display()
        ))
    })?)
    .map_err(|_| held())?;
    if attestation.expires_at <= Utc::now()
        || source_bytes > attestation.max_source_bytes
        || repositories.iter().any(|url| {
            !attestation
                .repositories
                .iter()
                .any(|allowed| allowed == url)
        })
    {
        return Err(held());
    }
    Ok(())
}
