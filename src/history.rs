use crate::config::{Config, Repository, env_name, identity, no_symlinks};
use crate::error::{Error, IoContext, Result};
use crate::registry::{Entry, Registry};
use crate::restic::{Restic, hex_digest, password};
use crate::selection::Export;
use crate::state::private_file;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    /// The IDs each Bot's shared repository has had. Version 1 has none, so a
    /// Bot absent here may have IDs recorded only in `repository_ids`.
    #[serde(default)]
    bot_repository_ids: BTreeMap<String, BTreeSet<String>>,
}

/// Repositories that `History::observe` accepts as missing (restic exit 10).
pub enum Missing<'a> {
    /// Any; a repository may not be initialized yet.
    Allowed,
    /// Only a previously shared local repository that moved: the candidate
    /// registry lists the same Bot at a repository that the history taken
    /// before this publish observed anything attributes to that Bot.
    Moved(&'a Registry, &'a History),
}

impl Missing<'_> {
    fn accepts(&self, bot: Option<&str>, config: &Config, restic: &Restic<'_>) -> bool {
        match self {
            Self::Allowed => true,
            Self::Moved(candidate, known) => {
                config.storage.is_local_backed()
                    && candidate.bots.iter().any(|entry| {
                        Some(entry.id.as_str()) == bot && known.attributes(entry, config, restic)
                    })
            }
        }
    }
}

impl History {
    pub fn new() -> Self {
        Self {
            version: 2,
            ..Self::default()
        }
    }

    /// Bytes that new restic master IDs add, `bot` naming the Bot whose
    /// pending shared repository is one of them.
    pub fn pending_master_bytes(&self, count: u64, bot: Option<&str>) -> u64 {
        // A JSON string ID is 66 bytes, plus one comma except for the first
        // item in an empty array. New restic master IDs have fixed width.
        let ids = count * 67 - u64::from(count > 0 && self.repository_ids.is_empty());
        // The Bot's ID is listed again under the Bot: `"<bot>":[<ID>]` if new.
        ids + bot.map_or(0, |bot| match self.bot_repository_ids.get(bot) {
            Some(listed) => 67 - u64::from(listed.is_empty()),
            None => bot.len() as u64 + 71 + u64::from(!self.bot_repository_ids.is_empty()),
        })
    }

    /// Whether `entry`'s repository opens with an ID of that Bot. A Bot
    /// without per-Bot IDs (recorded before version 2) is attributed a known
    /// ID only by its repository's snapshots, which all must be that Bot's.
    fn attributes(&self, entry: &Entry, config: &Config, restic: &Restic<'_>) -> bool {
        let repository = entry.repository();
        let Ok(id) = restic.repository_id(&repository) else {
            return false;
        };
        if let Some(ids) = self.bot_repository_ids.get(&entry.id) {
            return ids.contains(&id);
        }
        self.repository_ids.contains(&id)
            && restic
                .shared_repository_id()
                .is_ok_and(|shared| shared != id)
            && !self
                .bot_repository_ids
                .values()
                .any(|ids| ids.contains(&id))
            && restic.snapshots(&repository).is_ok_and(|snapshots| {
                !snapshots.is_empty()
                    && snapshots.iter().all(|snapshot| {
                        snapshot.tag("bot:") == Some(entry.id.as_str())
                            && snapshot.tag("hub:") == Some(config.hub.as_str())
                            && !snapshot
                                .tags
                                .iter()
                                .any(|tag| tag == "kind:shared" || tag == "kind:shared-v2")
                    })
            })
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
        let mut history: Self = serde_json::from_slice(
            &fs::read(&path).at("Cannot read shared identifier history", &path)?,
        )
        .map_err(|_| invalid())?;
        if !matches!(history.version, 1 | 2)
            || (history.version == 1 && !history.bot_repository_ids.is_empty())
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
            // A known ID has 64 hex digits; a Bot's IDs are also lowercase.
            || history.bot_repository_ids.iter().any(|(bot, ids)| {
                !identity(bot)
                    || ids.iter().any(|id| {
                        !history.repository_ids.contains(id)
                            || id.bytes().any(|c| c.is_ascii_uppercase())
                    })
            })
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
        // Version 1 loads as version 2 without per-Bot IDs; nothing writes version 1.
        history.version = 2;
        Ok(history)
    }

    pub fn observe(
        &mut self,
        registry: &Registry,
        config: &Config,
        restic: &Restic<'_>,
        missing: Missing<'_>,
    ) -> Result<()> {
        let repositories = std::iter::once((None, config.repositories.shared.clone())).chain(
            registry
                .bots
                .iter()
                .filter(|entry| entry.shared)
                .map(|entry| (Some(entry.id.as_str()), entry.repository())),
        );
        for (bot, repository) in repositories {
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
                        if let Some(bot) = bot {
                            self.bot_repository_ids
                                .entry(bot.to_owned())
                                .or_default()
                                .insert(id.clone());
                        }
                        self.repository_ids.insert(id);
                    }
                    Err(error) if error.code == 10 && missing.accepts(bot, config, restic) => (),
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

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
