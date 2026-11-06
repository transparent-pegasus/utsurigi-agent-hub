//! Per-machine live check state in `paths.state/live.json`.

use crate::config::no_symlinks;
use crate::error::{Error, Result};
use crate::live::format::{HubMark, Spec};
use crate::live::merge::Base;
use crate::state::write_private;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const FILE: &str = "live.json";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Live {
    /// Unix seconds; a check before this time is skipped.
    pub next_check: i64,
    pub changed_at: Option<i64>,
    pub failures: u32,
    pub failure: Option<String>,
    pub index: Option<Seen>,
    pub list: Option<Applied>,
    pub base: BTreeMap<String, Base>,
    /// List revision written but not yet recorded in the hub index.
    pub pending_index: Option<u64>,
    pub pending_hub: Option<HubMark>,
}

/// The index as last read: its token and hub revision.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seen {
    pub token: String,
    pub hub: u64,
}

/// The list this machine last applied.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    pub revision: u64,
    pub salt: String,
    pub spec: Spec,
}

impl Live {
    pub fn load(root: &Path) -> Result<Option<Self>> {
        let path = root.join(FILE);
        no_symlinks(&path)?;
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                Error::new(format!(
                    "Invalid live state {}; remove it to start live checks afresh",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::new(format!(
                "Cannot read live state {}: {error}",
                path.display()
            ))),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|_| Error::new("Cannot encode live state"))?;
        write_private(&root.join(FILE), &bytes)
    }
}
