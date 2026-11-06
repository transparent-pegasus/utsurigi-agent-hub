//! Live list and hub index wire formats.

use crate::error::{Error, Result};
use crate::live::policy::Policy;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};

pub const MAX_BYTES: u64 = 1 << 20;
pub const MAX_ENTRIES: usize = 2000;
pub const MAX_ROOTS: usize = 64;
const MAX_PATH_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub version: u32,
    pub revision: u64,
    pub salt: String,
    pub spec: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub roots: Vec<Root>,
    pub policy: Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub path: String,
    pub class: Class,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Task,
    Rule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub version: u32,
    pub hub: HubMark,
    pub bots: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubMark {
    pub revision: u64,
    pub snapshot: String,
}

impl List {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, "Invalid live list", |list: &Self| {
            let mut digests = HashSet::new();
            list.version == 1
                && list.revision >= 1
                && list.salt.len() == 32
                && is_hex(&list.salt)
                && !list.spec.is_empty()
                && is_hex(&list.spec)
                && list.entries.len() <= MAX_ENTRIES
                && list.entries.iter().all(|entry| {
                    !entry.name.is_empty()
                        && is_hex(&entry.name)
                        && is_digest(&entry.sha256)
                        && digests.insert(&entry.sha256)
                })
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        encode(self, "list")
    }
}

impl Index {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        decode(bytes, "Invalid live index", |index: &Self| {
            index.version == 1
                && index.hub.revision >= 1
                && is_digest(&index.hub.snapshot)
                && index
                    .bots
                    .iter()
                    .all(|(key, revision)| is_digest(key) && *revision >= 1)
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        encode(self, "index")
    }
}

impl Spec {
    pub fn validate(&self) -> Result<()> {
        let roots = &self.roots;
        if !(1..=MAX_ROOTS).contains(&roots.len())
            || roots.iter().any(|root| relative_path(&root.path).is_err())
            || roots.iter().enumerate().any(|(at, root)| {
                roots[at + 1..].iter().any(|other| {
                    root.path == other.path
                        || covers(&root.path, &other.path)
                        || covers(&other.path, &root.path)
                })
            })
        {
            return Err(Error::new("Invalid live roots"));
        }
        self.policy.validate()
    }
}

// "tasks" and "tasks/" name the same entry, so a directory root covers its bare name too.
fn covers(directory: &str, path: &str) -> bool {
    directory
        .strip_suffix('/')
        .is_some_and(|name| path == name || path.starts_with(directory))
}

pub fn bot_key(hub: &str, bot: &str) -> String {
    sha256_hex(
        ["utsurigi-agent-hub live v1", hub, bot]
            .join("\0")
            .as_bytes(),
    )
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn unhex(text: &str) -> Result<Vec<u8>> {
    let invalid = || Error::new("Invalid live hex");
    if !is_hex(text) {
        return Err(invalid());
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).map_err(|_| invalid()))
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn relative_path(path: &str) -> Result<()> {
    // An empty component also rejects "", a leading "/", and "//".
    if path.len() > MAX_PATH_BYTES
        || path.chars().any(|c| c == '\\' || c.is_control())
        || path
            .strip_suffix('/')
            .unwrap_or(path)
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::new("Invalid live path"));
    }
    Ok(())
}

fn is_hex(text: &str) -> bool {
    text.len().is_multiple_of(2)
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn is_digest(text: &str) -> bool {
    text.len() == 64 && is_hex(text)
}

fn decode<T: DeserializeOwned>(
    bytes: &[u8],
    invalid: &str,
    valid: impl FnOnce(&T) -> bool,
) -> Result<T> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::new(invalid));
    }
    serde_json::from_slice(bytes)
        .ok()
        .filter(valid)
        .ok_or_else(|| Error::new(invalid))
}

fn encode(value: &impl Serialize, what: &str) -> Result<Vec<u8>> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| Error::new(format!("Cannot encode live {what}")))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::new(format!("Live {what} exceeds its size limit")));
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
