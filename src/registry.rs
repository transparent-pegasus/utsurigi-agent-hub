use crate::config::{Config, Repository, env_name, identity, parse_yaml};
use crate::error::{Error, Result};
use crate::restic::{Restic, password};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    pub hub: String,
    pub owner: Owner,
    pub bots: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub bot_id: String,
    pub runtime_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub runtime_id: String,
    pub repository: String,
    pub password_ref: Option<String>,
    pub password_env: String,
    pub shared: bool,
}

impl Entry {
    pub fn repository(&self) -> Repository {
        Repository {
            url: Some(self.repository.clone()),
            password_ref: self.password_ref.clone(),
            password_env: Some(self.password_env.clone()),
        }
    }
}

impl Registry {
    pub fn load(root: &Path, config: &Config) -> Result<Self> {
        let path = root.join("registry.yaml");
        crate::config::no_symlinks(&path)?;
        let bytes =
            fs::read(path).map_err(|_| Error::new("A complete shared registry is required"))?;
        let registry: Self = parse_yaml(&bytes, "Invalid registry schema")?;
        if registry.hub != config.hub {
            return Err(Error::new("Registry hub does not match configuration"));
        }
        if registry.version != 1
            || registry.bots.is_empty()
            || registry.bots.len() > config.budget.max_bots
        {
            return Err(Error::new("Invalid registry version or fleet size"));
        }
        let mut ids = HashSet::new();
        let mut repositories = HashSet::new();
        for entry in &registry.bots {
            if !identity(&entry.id)
                || !identity(&entry.runtime_id)
                || !env_name(&entry.password_env)
                || !ids.insert(&entry.id)
                || !repositories.insert(&entry.repository)
                || Some(&entry.repository) == config.repositories.shared.url.as_ref()
            {
                return Err(Error::new(
                    "Invalid or duplicate registry identity/repository",
                ));
            }
            config.validate_url(&entry.repository)?;
        }
        if !registry.bots.iter().any(|entry| {
            entry.id == registry.owner.bot_id && entry.runtime_id == registry.owner.runtime_id
        }) {
            return Err(Error::new("Registry owner must name a current Bot runtime"));
        }
        Ok(registry)
    }

    pub fn entry(&self, id: &str) -> Result<&Entry> {
        self.bots
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| Error::new("Bot is not authorized by the current registry"))
    }

    pub fn is_owner(&self, config: &Config) -> bool {
        self.owner.bot_id == config.id() && self.owner.runtime_id == config.runtime_id()
    }

    pub fn current_runtime(&self, config: &Config) -> Result<&Entry> {
        let entry = self.entry(config.id())?;
        if entry.runtime_id != config.runtime_id() {
            return Err(Error::new(
                "This runtime was superseded; local work was preserved",
            ));
        }
        Ok(entry)
    }

    pub fn current(&self, config: &Config) -> Result<()> {
        let entry = self.current_runtime(config)?;
        if Some(&entry.repository) != config.repositories.bot.url.as_ref()
            || Some(&entry.password_env) != config.repositories.bot.password_env.as_ref()
            || entry.password_ref != config.repositories.bot.password_ref
            || entry.shared != config.sharing.bot_data
        {
            return Err(Error::new(
                "Bot repository and sharing configuration must match the current registry",
            ));
        }
        Ok(())
    }

    pub fn credentials(&self, config: &Config) -> Result<()> {
        password(&config.repositories.shared)?;
        password(&config.repositories.bot)?;
        let mut repositories = vec![
            (config.repositories.shared.clone(), true),
            (config.repositories.bot.clone(), config.sharing.bot_data),
        ];
        repositories.extend(
            self.bots
                .iter()
                .map(|entry| (entry.repository(), entry.shared)),
        );
        for (index, (repository, shared)) in repositories.iter().enumerate() {
            for (other, other_shared) in repositories.iter().skip(index + 1) {
                if repository.url == other.url || (*shared && *other_shared) {
                    continue;
                }
                if repository.password_env == other.password_env
                    || repository
                        .password_ref
                        .as_ref()
                        .filter(|reference| !reference.is_empty())
                        .is_some_and(|reference| Some(reference) == other.password_ref.as_ref())
                {
                    return Err(Error::new(
                        "Repositories with different access policies require separate password access",
                    ));
                }
                if let (Ok(value), Ok(other_value)) = (password(repository), password(other))
                    && value == other_value
                {
                    return Err(Error::new(
                        "Repositories with different access policies require different passwords",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn transition_from(
        &self,
        previous: &Self,
        config: &Config,
        restic: &Restic<'_>,
    ) -> Result<()> {
        self.credentials(config)?;
        for entry in self.bots.iter().filter(|entry| !entry.shared) {
            if previous
                .bots
                .iter()
                .any(|old| old.shared && old.repository == entry.repository)
            {
                return Err(Error::new(
                    "Shared-to-private transition requires a new repository and master key",
                ));
            }
            for old in previous.bots.iter().filter(|old| old.shared) {
                if (!old.password_env.is_empty() && old.password_env == entry.password_env)
                    || old
                        .password_ref
                        .as_ref()
                        .filter(|reference| !reference.is_empty())
                        .is_some_and(|reference| Some(reference) == entry.password_ref.as_ref())
                {
                    return Err(Error::new(
                        "Shared-to-private transition requires separate password access",
                    ));
                }
                let old_repository = old.repository();
                let repository = entry.repository();
                let old_password = password(&old_repository);
                let new_password = password(&repository);
                if old.id != entry.id && (old_password.is_err() || new_password.is_err()) {
                    continue;
                }
                if old_password? == new_password? {
                    return Err(Error::new(
                        "Shared-to-private transition requires a new password and master key",
                    ));
                }
                let old_id = restic.repository_id(&old_repository)?;
                match restic.repository_id(&repository) {
                    Ok(id) if id == old_id => {
                        return Err(Error::new(
                            "Repository reuse does not revoke a shared master key",
                        ));
                    }
                    Ok(_) => (),
                    Err(error) if error.code == 10 => (),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
}
