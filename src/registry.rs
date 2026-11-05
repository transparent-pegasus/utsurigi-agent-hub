use crate::config::{Config, Repository, identity, parse_yaml, password_env_name};
use crate::error::{Error, Result};
use crate::restic::{Restic, password};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub version: u32,
    pub hub: String,
    pub max_bots: usize,
    pub bots: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyRegistry {
    pub version: u32,
    pub hub: String,
    pub owner: Owner,
    pub bots: Vec<Entry>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub bot_id: String,
    pub runtime_id: String,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
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
    fn current(config: &Config) -> Self {
        Self {
            id: config.id().to_owned(),
            runtime_id: config.runtime_id().to_owned(),
            repository: config
                .repositories
                .bot
                .url
                .clone()
                .expect("runtime configuration"),
            password_ref: config.repositories.bot.password_ref.clone(),
            password_env: config
                .repositories
                .bot
                .password_env
                .clone()
                .expect("runtime configuration"),
            shared: config.sharing.bot_data,
        }
    }

    pub fn repository(&self) -> Repository {
        Repository {
            url: Some(self.repository.clone()),
            password_ref: self.password_ref.clone(),
            password_env: Some(self.password_env.clone()),
        }
    }
}

impl LegacyRegistry {
    pub fn load(root: &Path, config: &Config) -> Result<Self> {
        let registry: Self = parse_yaml(&registry_bytes(root)?, "Invalid legacy registry schema")?;
        if registry.version != 1
            || !registry.bots.iter().any(|entry| {
                entry.id == registry.owner.bot_id && entry.runtime_id == registry.owner.runtime_id
            })
        {
            return Err(Error::new(
                "Legacy registry owner must name a current Bot runtime",
            ));
        }
        registry.convert(config)?.validate(config)?;
        Ok(registry)
    }

    pub fn convert(&self, config: &Config) -> Result<Registry> {
        let registry = Registry {
            version: 2,
            hub: self.hub.clone(),
            max_bots: config.budget.max_bots,
            bots: self.bots.clone(),
        };
        registry.validate(config)?;
        Ok(registry)
    }
}

fn registry_bytes(root: &Path) -> Result<Vec<u8>> {
    let path = root.join("registry.yaml");
    crate::config::no_symlinks(&path)?;
    fs::read(path).map_err(|_| Error::new("A complete shared registry is required"))
}

impl Registry {
    pub fn load(root: &Path, config: &Config) -> Result<Self> {
        let registry: Self = parse_yaml(&registry_bytes(root)?, "Invalid registry v2 schema")?;
        registry.validate(config)?;
        Ok(registry)
    }

    pub fn initial(root: &Path, config: &Config, legacy_retry: bool) -> Result<Self> {
        let path = root.join("registry.yaml");
        crate::config::no_symlinks(&path)?;
        let mut registry = if !path.try_exists()? {
            Self {
                version: 2,
                hub: config.hub.clone(),
                max_bots: config.budget.max_bots,
                bots: vec![Entry::current(config)],
            }
        } else {
            let value: serde_json::Value =
                parse_yaml(&registry_bytes(root)?, "Invalid registry schema")?;
            if value["version"] == 1 {
                let legacy = LegacyRegistry::load(root, config)?;
                // An interrupted legacy initial backup binds its retry input to
                // that source runtime. An empty repository has no prior publisher.
                if legacy_retry
                    && (legacy.owner.bot_id != config.id()
                        || legacy.owner.runtime_id != config.runtime_id())
                {
                    return Err(Error::new(
                        "Initial legacy candidate source does not match this runtime; local work was preserved",
                    ));
                }
                legacy.convert(config)?
            } else {
                Self::load(root, config)?
            }
        };
        registry.max_bots = config.budget.max_bots;
        registry.register(config)?;
        registry.validate(config)?;
        Ok(registry)
    }

    fn validate(&self, config: &Config) -> Result<()> {
        if self.hub != config.hub {
            return Err(Error::new("Registry hub does not match configuration"));
        }
        if self.version != 2
            || self.max_bots == 0
            || self.bots.is_empty()
            || self.bots.len() > self.max_bots
        {
            return Err(Error::new("Invalid registry version or max_bots fleet cap"));
        }
        let mut ids = HashSet::new();
        let mut repositories = HashSet::new();
        for entry in &self.bots {
            if !identity(&entry.id)
                || !identity(&entry.runtime_id)
                || !password_env_name(&entry.password_env)
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
        Ok(())
    }

    pub fn register(&mut self, config: &Config) -> Result<bool> {
        if self.bots.iter().any(|entry| entry.id == config.id()) {
            self.current(config)?;
            return Ok(false);
        }
        if self.bots.len() >= self.max_bots {
            return Err(Error::new(
                "Shared max_bots fleet cap is full; local work was preserved",
            ));
        }
        self.bots.push(Entry::current(config));
        self.validate(config)?;
        Ok(true)
    }

    pub fn write(&self, export: &mut crate::selection::Export) -> Result<()> {
        let bytes =
            serde_saphyr::to_string(self).map_err(|_| Error::new("Cannot encode registry v2"))?;
        export.write("registry.yaml", bytes.as_bytes())
    }

    pub fn entry(&self, id: &str) -> Result<&Entry> {
        self.bots
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| {
                Error::new("Bot is absent from the current registry; run setup to register")
            })
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
        self.access_checks(config)
    }

    // Validate private access separation using available credentials. Optional
    // peer passwords, and an unrelated publisher's Bot password, are not required.
    pub fn access_checks(&self, config: &Config) -> Result<()> {
        password(&config.repositories.shared)?;
        let mut repositories = vec![(config.repositories.shared.clone(), true)];
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
        self.access_checks(config)?;
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
                if old_password.is_err() || new_password.is_err() {
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
