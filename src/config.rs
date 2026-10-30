use crate::error::{Error, Result};
use serde::{Deserialize, de::DeserializeOwned};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default = "default_hub")]
    pub hub: String,
    pub language: Option<String>,
    pub timezone: Option<String>,
    pub bot: Bot,
    pub machine: Machine,
    pub storage: Storage,
    pub secrets: Secrets,
    pub encryption: Encryption,
    pub repositories: Repositories,
    pub paths: Paths,
    pub data: Data,
    pub sharing: Sharing,
    pub sync: Sync,
    pub budget: Budget,
    pub tasks: Tasks,
    pub memory: Memory,
    pub skills: Skills,
    pub review: Review,
    pub onboarding: Onboarding,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bot {
    pub id: Option<String>,
    pub platform: Option<String>,
    pub runtime_id: Option<String>,
    pub role: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Machine {
    pub id: Option<String>,
    pub root: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub provider: String,
    pub class: String,
    pub credentials_ref: Option<String>,
    pub bot_transfer: Option<String>,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            provider: "r2".into(),
            class: "standard".into(),
            credentials_ref: None,
            bot_transfer: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Secrets {
    pub provider: String,
    pub confirm_each_access: bool,
}

impl Default for Secrets {
    fn default() -> Self {
        Self {
            provider: "1password".into(),
            confirm_each_access: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Encryption {
    pub executable: PathBuf,
    pub version: String,
}

impl Default for Encryption {
    fn default() -> Self {
        Self {
            executable: "restic".into(),
            version: "0.19.1".into(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Repositories {
    pub shared: Repository,
    pub bot: Repository,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Repository {
    pub url: Option<String>,
    pub password_ref: Option<String>,
    pub password_env: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub owned: Option<PathBuf>,
    pub shared: Option<PathBuf>,
    pub imports: Option<PathBuf>,
    pub state: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Data {
    pub include: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub sqlite_snapshots: Vec<PathBuf>,
    pub sync: Option<String>,
    pub reference: Option<String>,
    pub local: Option<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sharing {
    pub bot_data: bool,
}
impl Default for Sharing {
    fn default() -> Self {
        Self { bot_data: true }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sync {
    pub interval_seconds: u64,
    pub start_time: String,
    pub history_retention_seconds: u64,
    pub retry_lock_seconds: u64,
    pub transport_retries: u32,
    pub stuck_request_timeout_seconds: u64,
    pub upload_limit_kib: u64,
    pub download_limit_kib: u64,
    pub connections: u32,
    pub read_concurrency: u32,
}
impl Default for Sync {
    fn default() -> Self {
        Self {
            interval_seconds: 86400,
            start_time: "03:00".into(),
            history_retention_seconds: 2592000,
            retry_lock_seconds: 120,
            transport_retries: 5,
            stuck_request_timeout_seconds: 300,
            upload_limit_kib: 0,
            download_limit_kib: 0,
            connections: 5,
            read_concurrency: 2,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    pub free_tier_only: bool,
    pub max_bots: usize,
    pub storage_target_gb: u64,
    pub attestation_file: Option<PathBuf>,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            free_tier_only: true,
            max_bots: 50,
            storage_target_gb: 5,
            attestation_file: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tasks {
    pub enabled: bool,
}
impl Default for Tasks {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub enabled: bool,
    pub embedding_model: Option<String>,
    pub retrieval_bot_id: Option<String>,
    pub existing_source: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Skills {
    pub caveman: Caveman,
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Caveman {
    pub enabled: bool,
    pub source: String,
    pub r#ref: Option<String>,
}
impl Default for Caveman {
    fn default() -> Self {
        Self {
            enabled: true,
            source: "https://github.com/JuliusBrussee/caveman.git".into(),
            r#ref: None,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    pub reviewer_bot: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Onboarding {
    pub config_blob_url: Option<String>,
}

fn one() -> u32 {
    1
}

fn default_hub() -> String {
    "default".into()
}

pub fn identity(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

pub fn env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn relative(value: &Path) -> bool {
    !value.as_os_str().is_empty()
        && value
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

pub fn no_symlinks(path: &Path) -> Result<()> {
    let mut cursor = PathBuf::new();
    for component in path.components() {
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::new("Symlink paths are not allowed"));
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub fn absolute(base: &Path, value: &Path) -> Result<PathBuf> {
    if value.as_os_str().is_empty() {
        return Err(Error::new("Paths must not be empty"));
    }
    let joined = if value.is_absolute() {
        value.to_owned()
    } else {
        base.join(value)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => (),
            _ => normalized.push(component),
        }
    }
    no_symlinks(&joined)?;
    Ok(normalized)
}

pub fn parse_yaml<T: DeserializeOwned>(bytes: &[u8], invalid: &'static str) -> Result<T> {
    // Preserve YAML scalar types before applying the Rust schema.
    let options = serde_saphyr::options! { strict_booleans: true };
    let value: serde_json::Value =
        serde_saphyr::from_slice_with_options(bytes, options).map_err(|_| Error::new(invalid))?;
    serde_json::from_value(value).map_err(|_| Error::new(invalid))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let path = fs::canonicalize(path).map_err(|_| Error::new("Cannot read configuration"))?;
        let bytes = fs::read(&path).map_err(|_| Error::new("Cannot read configuration"))?;
        let mut config: Self = parse_yaml(
            &bytes,
            "Invalid configuration: unknown key, value, or YAML syntax",
        )?;
        let base = path
            .parent()
            .ok_or_else(|| Error::new("Invalid configuration path"))?;
        for path in [
            &mut config.machine.root,
            &mut config.paths.owned,
            &mut config.paths.shared,
            &mut config.paths.imports,
            &mut config.paths.state,
            &mut config.budget.attestation_file,
        ]
        .into_iter()
        .flatten()
        {
            *path = absolute(base, path)?;
        }
        if config.encryption.executable.components().count() > 1 {
            config.encryption.executable = absolute(base, &config.encryption.executable)?;
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !identity(&self.hub) {
            return Err(Error::new("Invalid hub name"));
        }
        if self.version != 1 || !["local", "r2"].contains(&self.storage.provider.as_str()) {
            return Err(Error::new(
                "Unsupported configuration version or storage provider",
            ));
        }
        if self.storage.class.trim().is_empty()
            || self.secrets.provider.trim().is_empty()
            || self.encryption.executable.as_os_str().is_empty()
            || self.encryption.version.split('.').count() != 3
            || self
                .encryption
                .version
                .split('.')
                .any(|n| n.is_empty() || n.parse::<u32>().is_err())
        {
            return Err(Error::new("Invalid provider or restic settings"));
        }
        for value in [&self.bot.id, &self.bot.runtime_id].into_iter().flatten() {
            if !identity(value) {
                return Err(Error::new("Invalid Bot identity"));
            }
        }
        if self
            .bot
            .platform
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
        {
            return Err(Error::new("Invalid Bot platform"));
        }
        utsurigi_agent_hub::schedule::first_anchor(
            0,
            self.timezone.as_deref().unwrap_or("UTC"),
            &self.sync.start_time,
        )?;
        if self.sync.interval_seconds == 0
            || self.sync.interval_seconds > i64::MAX as u64
            || self.sync.connections == 0
            || self.sync.read_concurrency == 0
            || self.sync.stuck_request_timeout_seconds == 0
            || self.budget.max_bots == 0
            || self.budget.storage_target_gb == 0
        {
            return Err(Error::new("Intervals and resource limits must be positive"));
        }
        for repo in [&self.repositories.shared, &self.repositories.bot] {
            if repo.password_env.as_ref().is_some_and(|s| !env_name(s)) {
                return Err(Error::new("Invalid password environment variable name"));
            }
            if let Some(url) = &repo.url {
                self.validate_url(url)?;
            }
        }
        if self.repositories.shared.url.is_some()
            && self.repositories.shared.url == self.repositories.bot.url
        {
            return Err(Error::new("Shared and Bot repositories must be separate"));
        }
        if self.storage.provider == "local"
            && let (Some(shared), Some(bot)) =
                (&self.repositories.shared.url, &self.repositories.bot.url)
            && (Path::new(shared).starts_with(bot) || Path::new(bot).starts_with(shared))
        {
            return Err(Error::new("Local repositories must not overlap"));
        }
        let roots: Vec<_> = [
            &self.paths.owned,
            &self.paths.shared,
            &self.paths.imports,
            &self.paths.state,
        ]
        .into_iter()
        .flatten()
        .collect();
        for (i, root) in roots.iter().enumerate() {
            if root.parent().is_none() {
                return Err(Error::new("A filesystem root cannot be a data directory"));
            }
            for other in roots.iter().skip(i + 1) {
                if root.starts_with(other) || other.starts_with(root) {
                    return Err(Error::new(
                        "Owned, shared, imports, and state roots must not overlap",
                    ));
                }
            }
        }
        for path in self.data.include.iter().chain(&self.data.sqlite_snapshots) {
            if !relative(path) {
                return Err(Error::new("Data paths must stay beneath the owned root"));
            }
            if let Some(root) = &self.paths.owned {
                no_symlinks(&root.join(path))?;
            }
        }
        if self.data.sqlite_snapshots.iter().any(|p| {
            !self
                .data
                .include
                .iter()
                .any(|i| p.starts_with(i) || i == Path::new("."))
        }) {
            return Err(Error::new("SQLite snapshots must be included data paths"));
        }
        for pattern in &self.data.exclude {
            if !relative(Path::new(pattern)) || globset::Glob::new(pattern).is_err() {
                return Err(Error::new("Invalid data exclusion pattern"));
            }
        }
        Ok(())
    }

    pub fn validate_url(&self, url: &str) -> Result<()> {
        if url.is_empty() || url.contains(['\n', '\r', '\0']) {
            return Err(Error::new("Invalid repository URL"));
        }
        if self.storage.provider == "local" {
            if !Path::new(url).is_absolute()
                || Path::new(url)
                    .components()
                    .any(|c| c == Component::ParentDir)
            {
                return Err(Error::new("Local repository URLs must be absolute paths"));
            }
            no_symlinks(Path::new(url))?;
            for root in [
                &self.paths.owned,
                &self.paths.shared,
                &self.paths.imports,
                &self.paths.state,
            ]
            .into_iter()
            .flatten()
            {
                if root.starts_with(url) || Path::new(url).starts_with(root) {
                    return Err(Error::new("Repositories must not overlap data directories"));
                }
            }
        } else if !url
            .strip_prefix("s3:https://")
            .and_then(|s| s.split_once('/'))
            .is_some_and(|(host, path)| !host.is_empty() && !path.trim_matches('/').is_empty())
            || url.contains(['@', '?', '#'])
            || url.chars().any(char::is_whitespace)
        {
            return Err(Error::new(
                "R2 repositories require an HTTPS S3 repository URL without credentials",
            ));
        }
        Ok(())
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        for (value, key) in [
            (&self.bot.id, "bot.id"),
            (&self.bot.platform, "bot.platform"),
            (&self.bot.runtime_id, "bot.runtime_id"),
            (&self.timezone, "timezone"),
            (&self.repositories.shared.url, "repositories.shared.url"),
            (
                &self.repositories.shared.password_env,
                "repositories.shared.password_env",
            ),
            (&self.repositories.bot.url, "repositories.bot.url"),
            (
                &self.repositories.bot.password_env,
                "repositories.bot.password_env",
            ),
        ] {
            if value.is_none() {
                missing.push(key);
            }
        }
        for (value, key) in [
            (&self.paths.owned, "paths.owned"),
            (&self.paths.shared, "paths.shared"),
            (&self.paths.imports, "paths.imports"),
            (&self.paths.state, "paths.state"),
        ] {
            if value.is_none() {
                missing.push(key);
            }
        }
        if self.data.include.is_empty() {
            missing.push("data.include");
        }
        missing
    }

    pub fn runtime(&self) -> Result<()> {
        let missing = self.missing();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::new(format!(
                "Missing configuration: {}",
                missing.join(", ")
            )))
        }
    }

    pub fn id(&self) -> &str {
        self.bot.id.as_deref().expect("runtime configuration")
    }
    pub fn runtime_id(&self) -> &str {
        self.bot
            .runtime_id
            .as_deref()
            .expect("runtime configuration")
    }
}
