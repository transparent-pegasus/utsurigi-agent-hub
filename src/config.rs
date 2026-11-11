use crate::error::{Error, IoContext, Result};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const CHILD_ENV: [&str; 14] = [
    "PATH",
    "SYSTEMROOT",
    "WINDIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
];

pub const RESTIC_TLS_ENV: [&str; 2] = ["RESTIC_CACERT", "RESTIC_TLS_CLIENT_CERT"];

pub const STORAGE_CREDENTIAL_ENV: [&str; 3] = [
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
    #[serde(skip)]
    pub source_digest: Option<String>,
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
    // Parse old toggles for compatibility; task management is now required.
    #[serde(rename = "tasks")]
    pub _legacy_tasks: LegacyTasks,
    pub memory: Memory,
    pub skills: Skills,
    pub review: Review,
    pub onboarding: Onboarding,
}

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Bot {
    pub id: Option<String>,
    pub platform: Option<String>,
    pub runtime_id: Option<String>,
    pub role: Option<String>,
}

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Machine {
    pub id: Option<String>,
    pub root: Option<PathBuf>,
}

#[derive(Deserialize, Debug, PartialEq)]
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
            provider: "google-drive".into(),
            class: "standard".into(),
            credentials_ref: None,
            bot_transfer: None,
        }
    }
}

impl Storage {
    /// Providers whose restic repositories are local paths. For `google-drive`
    /// the Bot moves the encrypted repositories through its connector.
    pub fn is_local_backed(&self) -> bool {
        matches!(self.provider.as_str(), "google-drive" | "local")
    }
}

#[derive(Deserialize, Debug, PartialEq)]
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

#[derive(Deserialize, Debug, PartialEq)]
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

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Repositories {
    pub shared: Repository,
    pub bot: Repository,
}

#[derive(Clone, Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Repository {
    pub url: Option<String>,
    pub password_ref: Option<String>,
    pub password_env: Option<String>,
}

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    pub owned: Option<PathBuf>,
    pub shared: Option<PathBuf>,
    pub imports: Option<PathBuf>,
    pub state: Option<PathBuf>,
}

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Data {
    pub include: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub sqlite_snapshots: Vec<PathBuf>,
    pub sync: Option<String>,
    pub reference: Option<String>,
    pub local: Option<String>,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Sharing {
    pub bot_data: bool,
}
impl Default for Sharing {
    fn default() -> Self {
        Self { bot_data: true }
    }
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Sync {
    pub interval_seconds: u64,
    pub start_time: String,
    pub versioning: bool,
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
            versioning: true,
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

#[derive(Deserialize, Debug, PartialEq)]
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

#[derive(Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct LegacyTasks {
    #[serde(rename = "enabled")]
    pub _enabled: bool,
}
impl Default for LegacyTasks {
    fn default() -> Self {
        Self { _enabled: true }
    }
}

#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub enabled: bool,
    pub embedding_model: Option<String>,
    pub retrieval_bot_id: Option<String>,
    pub existing_source: Option<String>,
}
#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Skills {
    pub caveman: Caveman,
}
#[derive(Deserialize, Debug, PartialEq)]
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
#[derive(Default, Deserialize, Debug, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    pub reviewer_bot: Option<String>,
}
#[derive(Default, Deserialize, Debug, PartialEq)]
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

pub fn password_env_name(value: &str) -> bool {
    env_name(value)
        && !CHILD_ENV.contains(&value)
        && !RESTIC_TLS_ENV.contains(&value)
        && !STORAGE_CREDENTIAL_ENV.contains(&value)
}

pub fn dotenv(canonical_config: &Path) -> Result<BTreeMap<String, String>> {
    let path = canonical_config.with_file_name(".env");
    let insecure = || {
        Error::new(
            "Config-adjacent .env must be an owner-owned regular file with mode 0600 and no symlinks",
        )
    };
    no_symlinks(&path).map_err(|_| insecure())?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(insecure()),
    };
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(Error::new(
            "Private .env permissions require a supported Unix host",
        ))
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;

        let private = |metadata: &fs::Metadata| {
            metadata.is_file()
                && metadata.mode() & 0o7777 == 0o600
                && metadata.uid() == unsafe { libc::geteuid() }
        };
        if !private(&metadata) {
            return Err(insecure());
        }
        let mut file = crate::selection::open_source(
            canonical_config.parent().ok_or_else(insecure)?,
            Path::new(".env"),
        )
        .map_err(|_| insecure())?;
        if !private(&file.metadata().map_err(|_| insecure())?) {
            return Err(insecure());
        }
        let invalid = || Error::new("Invalid config-adjacent .env syntax");
        let mut document = String::new();
        file.read_to_string(&mut document).map_err(|_| invalid())?;
        if document.contains('\0') {
            return Err(invalid());
        }
        let mut variables = BTreeMap::new();
        for line in document.lines() {
            if line.contains('\r') {
                return Err(invalid());
            }
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line
                .strip_prefix("export")
                .filter(|rest| rest.starts_with(char::is_whitespace))
                .map_or(line, str::trim_start);
            let (name, value) = line.split_once('=').ok_or_else(invalid)?;
            let name = name.trim();
            if !env_name(name) {
                return Err(invalid());
            }
            let value = value.trim();
            let value = if let Some(quote @ ('\'' | '"')) = value.chars().next() {
                value[1..].strip_suffix(quote).ok_or_else(invalid)?
            } else {
                value
            };
            if variables
                .insert(name.to_owned(), value.to_owned())
                .is_some()
            {
                return Err(invalid());
            }
        }
        Ok(variables)
    }
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
                return Err(Error::new(format!(
                    "Symlink paths are not allowed: {}",
                    cursor.display()
                )));
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => {
                return Err(Error::new(format!(
                    "Cannot inspect path {}: {e}",
                    cursor.display()
                )));
            }
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
        let path = fs::canonicalize(path).at("Cannot read configuration", path)?;
        let bytes = fs::read(&path).at("Cannot read configuration", &path)?;
        let mut config: Self = parse_yaml(
            &bytes,
            "Invalid configuration: unknown key, value, or YAML syntax",
        )?;
        config.source_path = Some(path.clone());
        config.source_digest = Some(format!("{:x}", Sha256::digest(&bytes)));
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
        for repository in [
            &mut config.repositories.shared,
            &mut config.repositories.bot,
        ] {
            if repository.password_ref.as_deref() == Some("") {
                repository.password_ref = None;
            }
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if !identity(&self.hub) {
            return Err(Error::new("Invalid hub name"));
        }
        if self.version != 1
            || !["google-drive", "r2", "local"].contains(&self.storage.provider.as_str())
        {
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
        crate::schedule::first_anchor(
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
            if repo
                .password_env
                .as_ref()
                .is_some_and(|s| !password_env_name(s))
            {
                return Err(Error::new(
                    "Invalid password environment variable name: connection, storage credential and system names are reserved",
                ));
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
        if self.storage.is_local_backed()
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
        if self.storage.is_local_backed() {
            if url.starts_with("s3:") {
                return Err(Error::new(
                    "S3 repository URLs need storage.provider: r2; the default is now google-drive",
                ));
            }
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
        } else if url
            .strip_prefix("s3:https://")
            .and_then(|repository| repository.split_once('/'))
            .is_some_and(|(host, path)| {
                host.split('.')
                    .chain(path.split('/'))
                    .any(|component| matches!(component, "ACCOUNT" | "BUCKET"))
            })
        {
            return Err(Error::new(
                "Replace ACCOUNT and BUCKET repository URL placeholders",
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

    // Hub-scoped: registries never carry a raw machine ID or link one machine across hubs.
    pub fn machine_hash(&self) -> Option<String> {
        let id = self.machine.id.as_deref()?;
        let digest = Sha256::new()
            .chain_update(b"utsurigi-agent-hub machine v1\0")
            .chain_update(&self.hub)
            .chain_update(b"\0")
            .chain_update(id)
            .finalize();
        Some(format!("{digest:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_fields_match_the_complete_public_template() {
        let omitted: Config =
            parse_yaml(b"{}", "empty config").unwrap_or_else(|e| panic!("{}", e.message));
        let template: Config = parse_yaml(include_bytes!("../config.yaml"), "template")
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(omitted, template);
    }

    #[test]
    fn google_drive_validates_like_local() {
        let config = |provider: &str| {
            let mut config = Config {
                version: 1,
                hub: "default".into(),
                ..Config::default()
            };
            config.storage.provider = provider.into();
            config
        };
        for provider in ["google-drive", "local"] {
            let mut config = config(provider);
            for url in ["relative/shared", "/tmp/a/../shared"] {
                assert!(config.validate_url(url).is_err(), "{provider}: {url}");
            }
            assert_eq!(
                config
                    .validate_url("s3:https://x.invalid/b/p")
                    .unwrap_err()
                    .message,
                "S3 repository URLs need storage.provider: r2; the default is now google-drive"
            );
            config.repositories.shared.url = Some("/nonexistent-uah/repos/shared".into());
            config.repositories.bot.url = Some("/nonexistent-uah/repos/shared/bot".into());
            assert!(config.validate().is_err(), "{provider}: overlap accepted");
            config.repositories.bot.url = Some("/nonexistent-uah/repos/bot".into());
            config
                .validate()
                .unwrap_or_else(|e| panic!("{provider}: {}", e.message));
        }
        assert_eq!(
            config("s3").validate().unwrap_err().message,
            "Unsupported configuration version or storage provider"
        );
    }

    #[test]
    fn google_drive_needs_no_cloud_attestation() {
        let config = Config::default();
        assert!(config.budget.free_tier_only && config.budget.attestation_file.is_none());
        crate::budget::authorize(&config, &["/nonexistent-uah/repos/bot"], 1 << 30)
            .unwrap_or_else(|e| panic!("{}", e.message));
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod machine_tests;
