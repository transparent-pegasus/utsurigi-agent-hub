use crate::config::{Config, Repository, env_name, no_symlinks};
use crate::error::{Error, IoContext, Result};
use crate::registry::Registry;
use crate::restic::{Restic, hex_digest, password};
use crate::selection::Export;
use crate::state::private_file;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const FILE: &str = "registry-history.json";

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct History {
    version: u32,
    repository_urls: BTreeSet<String>,
    password_refs: BTreeSet<String>,
    password_envs: BTreeSet<String>,
    repository_ids: BTreeSet<String>,
}

impl History {
    pub fn new() -> Self {
        Self {
            version: 1,
            ..Self::default()
        }
    }

    pub fn pending_master_bytes(&self, count: u64) -> u64 {
        // A JSON string ID is 66 bytes, plus one comma except for the first
        // item in an empty array. New restic master IDs have fixed width.
        count * 67 - u64::from(count > 0 && self.repository_ids.is_empty())
    }

    pub fn load(root: &Path, registry: &Registry) -> Result<Self> {
        let invalid = || Error::new("Missing or invalid shared identifier history");
        let path = root.join(FILE);
        no_symlinks(&path)?;
        let metadata = match fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(invalid()),
            result => result.at("Cannot inspect shared identifier history", &path)?,
        };
        if !metadata.is_file() {
            return Err(invalid());
        }
        let history: Self = serde_json::from_slice(
            &fs::read(&path).at("Cannot read shared identifier history", &path)?,
        )
        .map_err(|_| invalid())?;
        if history.version != 1
            || history.repository_urls.is_empty()
            || history.password_envs.is_empty()
            || history.repository_ids.is_empty()
            || history
                .repository_urls
                .iter()
                .any(|url| url.is_empty() || url.contains(['\n', '\r', '\0']))
            || history
                .password_refs
                .iter()
                .any(|reference| reference.is_empty())
            || history.password_envs.iter().any(|name| !env_name(name))
            || history.repository_ids.iter().any(|id| !hex_digest(id))
        {
            return Err(invalid());
        }
        for entry in registry.bots.iter().filter(|entry| entry.shared) {
            if !history.repository_urls.contains(&entry.repository)
                || !history.password_envs.contains(&entry.password_env)
                || entry
                    .password_ref
                    .as_ref()
                    .filter(|reference| !reference.is_empty())
                    .is_some_and(|reference| !history.password_refs.contains(reference))
            {
                return Err(invalid());
            }
        }
        Ok(history)
    }

    pub fn observe(
        &mut self,
        registry: &Registry,
        config: &Config,
        restic: &Restic<'_>,
        allow_missing: bool,
    ) -> Result<()> {
        let repositories = std::iter::once(config.repositories.shared.clone()).chain(
            registry
                .bots
                .iter()
                .filter(|entry| entry.shared)
                .map(|entry| entry.repository()),
        );
        for repository in repositories {
            self.repository_urls.insert(
                repository
                    .url
                    .as_ref()
                    .expect("runtime configuration")
                    .clone(),
            );
            if let Some(reference) = repository
                .password_ref
                .as_ref()
                .filter(|value| !value.is_empty())
            {
                self.password_refs.insert(reference.clone());
            }
            self.password_envs.insert(
                repository
                    .password_env
                    .as_ref()
                    .expect("runtime configuration")
                    .clone(),
            );
            if password(&repository).is_ok() {
                match restic.repository_id(&repository) {
                    Ok(id) => {
                        self.repository_ids.insert(id);
                    }
                    Err(error) if allow_missing && error.code == 10 => (),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    pub fn check(&self, registry: &Registry, restic: &Restic<'_>) -> Result<()> {
        for entry in registry.bots.iter().filter(|entry| !entry.shared) {
            self.check_private(&entry.repository(), restic)?;
        }
        Ok(())
    }

    pub fn check_private(&self, repository: &Repository, restic: &Restic<'_>) -> Result<()> {
        if repository
            .url
            .as_ref()
            .is_some_and(|url| self.repository_urls.contains(url))
            || repository
                .password_env
                .as_ref()
                .is_some_and(|name| self.password_envs.contains(name))
            || repository
                .password_ref
                .as_ref()
                .filter(|reference| !reference.is_empty())
                .is_some_and(|reference| self.password_refs.contains(reference))
        {
            return Err(Error::new(
                "Shared-to-private transition requires a new repository and separate password access",
            ));
        }
        if let Ok(value) = password(repository) {
            if self.password_envs.iter().any(|name| {
                std::env::var_os(name)
                    .filter(|value| !value.is_empty())
                    .is_some_and(|shared| shared == value)
            }) {
                return Err(Error::new(
                    "Shared-to-private transition requires a new password and master key",
                ));
            }
            match restic.repository_id(repository) {
                Ok(id) if self.repository_ids.contains(&id) => {
                    return Err(Error::new(
                        "Private Bot data requires a separate repository and master key",
                    ));
                }
                Ok(_) => (),
                Err(error) if error.code == 10 => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub fn write(&self, export: &mut Export) -> Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| Error::new("Cannot encode shared identifier history"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(export.tree.path().join(FILE))
            .map_err(|error| {
                Error::new(format!(
                    "registry-history.json is reserved generated shared metadata: {}: {error}",
                    export.tree.path().join(FILE).display()
                ))
            })?;
        private_file(&file, &export.tree.path().join(FILE))?;
        file.write_all(&bytes).at(
            "Cannot write shared identifier history",
            &export.tree.path().join(FILE),
        )?;
        export.bytes = export
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| Error::new("Source size overflow"))?;
        Ok(())
    }
}
