use crate::config::{CHILD_ENV, Config, RESTIC_TLS_ENV, Repository};
use crate::error::{Error, Result};
use crate::selection::{Export, validate_import};
use crate::state::{private_dir, private_file};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub struct Restic<'a> {
    config: &'a Config,
    executable: PathBuf,
    cache: PathBuf,
    temp: PathBuf,
    inherited_shared_password: Option<OsString>,
}

#[derive(Deserialize)]
pub struct Snapshot {
    // `cat snapshot` omits the ID; `snapshots` validates it separately.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct BackupMessage {
    message_type: String,
    snapshot_id: Option<String>,
}

fn backup_snapshot_id(bytes: &[u8]) -> Result<String> {
    let invalid = || {
        Error::new(
            "Invalid restic backup summary; no completion marker was added; no write was replayed",
        )
    };
    let mut id = None;
    for message in serde_json::Deserializer::from_slice(bytes).into_iter::<BackupMessage>() {
        let message = message.map_err(|_| invalid())?;
        if id.is_some() {
            return Err(invalid());
        }
        match message.message_type.as_str() {
            "summary" => {
                id = Some(
                    message
                        .snapshot_id
                        .filter(|id| snapshot_id(id))
                        .ok_or_else(invalid)?,
                );
            }
            "status" | "verbose_status" => (),
            _ => return Err(invalid()),
        }
    }
    id.ok_or_else(invalid)
}

impl Snapshot {
    pub fn completed_at(&self) -> Option<i64> {
        let mut markers = self
            .tags
            .iter()
            .filter_map(|s| s.strip_prefix("completed:"));
        let time = markers.next()?.parse().ok()?;
        if markers.next().is_some() {
            return None;
        }
        Some(time)
    }

    pub fn kind(&self, kind: &str) -> bool {
        self.tags.iter().any(|s| s == "utsurigi-agent-hub")
            && self.tags.iter().any(|s| s == &format!("kind:{kind}"))
    }

    pub fn owned_by(&self, id: &str, kind: &str) -> bool {
        self.kind(kind)
            && self.hostname == id
            && self.tags.iter().any(|s| s == &format!("bot:{id}"))
    }
}

pub fn password(repository: &Repository) -> Result<OsString> {
    let name = repository
        .password_env
        .as_deref()
        .ok_or_else(|| Error::new("Missing repository password environment variable name"))?;
    std::env::var_os(name)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::new("Required repository password was not delivered by the host"))
}

fn safe_environment(command: &mut Command) {
    command
        .env_clear()
        .stdin(Stdio::null())
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC");
    for name in CHILD_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

fn compatible(path: &Path, version: &str) -> bool {
    let mut command = Command::new(path);
    safe_environment(&mut command);
    command.arg("version");
    command.output().ok().is_some_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .take(2)
                .eq(["restic", version])
    })
}

fn find_executable(path: &Path) -> Option<PathBuf> {
    if path.components().count() > 1 {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::var_os("PATH").and_then(|value| {
        std::env::split_paths(&value)
            .map(|dir| dir.join(path))
            .find(|candidate| candidate.is_file())
    })
}

fn sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "token",
        "secret",
        "credential",
        "access_key",
        "api_key",
        "authorization",
    ]
    .iter()
    .any(|label| name.contains(label))
}

fn credential_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-')
}

fn redact_value(diagnostic: &str, value: &str) -> String {
    let mut redacted = String::new();
    let mut copied = 0;
    for (start, _) in diagnostic.match_indices(value) {
        let end = start + value.len();
        if diagnostic[..start]
            .chars()
            .next_back()
            .is_some_and(credential_char)
            || diagnostic[end..]
                .chars()
                .next()
                .is_some_and(credential_char)
        {
            continue;
        }
        redacted.push_str(&diagnostic[copied..start]);
        redacted.push_str("[redacted]");
        copied = end;
    }
    redacted.push_str(&diagnostic[copied..]);
    redacted
}

fn credential_end(value: &str) -> usize {
    if let Some(quote @ ('\'' | '"')) = value.chars().next() {
        let mut escaped = false;
        for (index, c) in value[1..].char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == quote {
                return index + 2;
            }
        }
        return value.len();
    }
    // Bare values may contain spaces and punctuation; only quotes give a safe end.
    value.len()
}

fn redact_credentials(line: &str) -> String {
    let mut redacted = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while cursor < line.len() {
        let c = line[cursor..].chars().next().expect("character boundary");
        if !credential_char(c) {
            cursor += c.len_utf8();
            continue;
        }
        let start = cursor;
        cursor += line[cursor..]
            .find(|c| !credential_char(c))
            .unwrap_or(line.len() - cursor);
        let name = &line[start..cursor];
        let mut rest = &line[cursor..];
        if let Some(quote @ ('\'' | '"')) = line[..start].chars().next_back()
            && rest.starts_with(quote)
        {
            rest = &rest[1..];
        }
        rest = rest.trim_start();
        let assignment = rest.starts_with(['=', ':']);
        if assignment {
            rest = rest[1..].trim_start();
        }
        let normalized = name.replace('-', "_").to_ascii_lowercase();
        let prefix = line[..start].trim_end();
        let bare_label = normalized.split('_').any(|part| {
            matches!(
                part,
                "password"
                    | "passwd"
                    | "token"
                    | "secret"
                    | "credential"
                    | "credentials"
                    | "authorization"
            )
        }) || normalized.contains("access_key")
            || normalized.contains("api_key");
        // Bare fields start a clause. Common prose continues after the label.
        let bare_field = bare_label
            && line[cursor..].starts_with(char::is_whitespace)
            && (prefix.is_empty() || prefix.ends_with([':', ';', ',', '{', '[']))
            && !matches!(
                rest.split_whitespace().next(),
                Some("or" | "and" | "is" | "are" | "was" | "were" | "file" | "files")
            );
        if (sensitive_name(&normalized)
            && (assignment
                || name.starts_with("--")
                || rest.starts_with(['\'', '"'])
                || bare_field))
            || name.eq_ignore_ascii_case("bearer")
        {
            let value_start = line.len() - rest.len();
            let end = value_start + credential_end(rest);
            redacted.push_str(&line[copied..value_start]);
            redacted.push_str("[redacted]");
            copied = end;
            cursor = end;
        }
    }
    redacted.push_str(&line[copied..]);
    redacted
}

fn stderr_line(
    config: &Config,
    repository: &Repository,
    inherited_password: Option<&OsString>,
    bytes: &[u8],
) -> String {
    let mut private_values: Vec<String> = std::env::vars_os()
        .filter(|(name, _)| sensitive_name(&name.to_string_lossy()))
        .map(|(_, value)| value.to_string_lossy().into_owned())
        .collect();
    if let Some(value) = inherited_password {
        private_values.push(value.to_string_lossy().into_owned());
    }
    for repository in [
        repository,
        &config.repositories.shared,
        &config.repositories.bot,
    ] {
        private_values.extend(repository.url.iter().cloned());
        private_values.extend(repository.password_ref.iter().cloned());
        if let Some(value) = repository
            .password_env
            .as_deref()
            .and_then(std::env::var_os)
        {
            private_values.push(value.to_string_lossy().into_owned());
        }
    }
    private_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut diagnostic = String::from_utf8_lossy(bytes).into_owned();
    for value in private_values.iter().filter(|value| !value.is_empty()) {
        diagnostic = redact_value(&diagnostic, value);
    }
    let first: String = diagnostic
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let diagnostic = redact_credentials(&first);
    let mut line = diagnostic
        .split_whitespace()
        .map(|word| {
            if word.contains("://") || word.contains('@') || word.starts_with("s3:") {
                let body = word.trim_end_matches([';', ',', ')', ']', '}', '\'', '"']);
                format!("[redacted]{}", &word[body.len()..])
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return "no diagnostic".into();
    }
    if line.len() > 512 {
        let mut end = 512 - "...".len();
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
        line.push_str("...");
    }
    line
}

impl<'a> Restic<'a> {
    pub fn open(config: &'a Config, acquire: bool) -> Result<Self> {
        let state = config.paths.state.as_ref().expect("runtime configuration");
        let installed = state
            .join("bin")
            .join(format!("restic-{}", config.encryption.version));
        let configured = find_executable(&config.encryption.executable);
        let executable = if let Some(path) =
            configured.filter(|p| compatible(p, &config.encryption.version))
        {
            path
        } else if config.encryption.executable.components().count() == 1
            && compatible(&installed, &config.encryption.version)
        {
            installed
        } else if acquire {
            let target = if config.encryption.executable.components().count() > 1 {
                config.encryption.executable.clone()
            } else {
                installed
            };
            if target.exists() {
                return Err(Error::new(
                    "Configured restic is incompatible; preserve it and choose a new executable path",
                ));
            }
            acquire_official(&target, &config.encryption.version)?;
            target
        } else {
            return Err(Error::new("Compatible restic is unavailable; run setup"));
        };
        let cache = state.join("cache");
        let temp = state.join("tmp");
        private_dir(&cache)?;
        private_dir(&temp)?;
        Ok(Self {
            config,
            executable,
            cache,
            temp,
            inherited_shared_password: None,
        })
    }

    pub(crate) fn inherit_shared_password(&mut self, password: OsString) {
        self.inherited_shared_password = Some(password);
    }

    fn output(
        &self,
        repository: &Repository,
        arguments: &[OsString],
        cwd: Option<&Path>,
    ) -> Result<Output> {
        let mut command = Command::new(&self.executable);
        safe_environment(&mut command);
        for name in RESTIC_TLS_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let repository_password = if repository.url == self.config.repositories.shared.url {
            self.inherited_shared_password
                .clone()
                .map(Ok)
                .unwrap_or_else(|| password(repository))?
        } else {
            password(repository)?
        };
        command
            .env("RESTIC_PASSWORD", repository_password)
            .env("TMPDIR", &self.temp)
            .env("TMP", &self.temp)
            .env("TEMP", &self.temp)
            .arg("--repo")
            .arg(
                repository
                    .url
                    .as_ref()
                    .ok_or_else(|| Error::new("Missing repository URL"))?,
            )
            .arg("--cache-dir")
            .arg(&self.cache)
            .arg("--retry-lock")
            .arg(format!("{}s", self.config.sync.retry_lock_seconds))
            .arg("--stuck-request-timeout")
            .arg(format!(
                "{}s",
                self.config.sync.stuck_request_timeout_seconds
            ))
            .arg("--limit-upload")
            .arg(self.config.sync.upload_limit_kib.to_string())
            .arg("--limit-download")
            .arg(self.config.sync.download_limit_kib.to_string());
        if self.config.storage.provider == "r2" {
            for name in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] {
                let value = std::env::var_os(name)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| Error::new("R2 credentials were not delivered by the host"))?;
                command.env(name, value);
            }
            if let Some(value) = std::env::var_os("AWS_SESSION_TOKEN") {
                command.env("AWS_SESSION_TOKEN", value);
            }
            command
                .arg("-o")
                .arg("s3.region=auto")
                .arg("-o")
                .arg(format!("s3.retries={}", self.config.sync.transport_retries))
                .arg("-o")
                .arg(format!("s3.connections={}", self.config.sync.connections));
        } else {
            command.arg("-o").arg(format!(
                "local.connections={}",
                self.config.sync.connections
            ));
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .args(arguments)
            .output()
            .map_err(|_| Error::new("Cannot execute restic"))
    }

    pub fn run(
        &self,
        repository: &Repository,
        arguments: &[OsString],
        cwd: Option<&Path>,
    ) -> Result<Vec<u8>> {
        let output = self.output(repository, arguments, cwd)?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(1);
            let meaning = match code {
                3 => "source files could not be read",
                10 => "repository does not exist",
                11 => "repository is locked",
                12 => "incorrect password",
                _ => "operation failed",
            };
            let kind = if repository.url == self.config.repositories.shared.url {
                "shared"
            } else {
                "bot"
            };
            let diagnostic = stderr_line(
                self.config,
                repository,
                self.inherited_shared_password.as_ref(),
                &output.stderr,
            );
            return Err(Error {
                message: format!(
                    "restic {kind} repository operation failed (exit {code}: {meaning}); no write was replayed; stderr: {diagnostic}"
                ),
                code,
            });
        }
        Ok(output.stdout)
    }

    pub(crate) fn raw_snapshots(&self, repository: &Repository) -> Result<Vec<Snapshot>> {
        let bytes = self.run(
            repository,
            &["snapshots".into(), "--json".into(), "--no-lock".into()],
            None,
        )?;
        let snapshots: Vec<Snapshot> = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("Invalid restic snapshot response"))?;
        if snapshots.iter().any(|s| !snapshot_id(&s.id)) {
            return Err(Error::new("Invalid restic snapshot identifier"));
        }
        Ok(snapshots)
    }

    pub fn snapshots(&self, repository: &Repository) -> Result<Vec<Snapshot>> {
        let snapshots = self.raw_snapshots(repository)?;
        for snapshot in &snapshots {
            let legacy_hub = snapshot
                .tags
                .iter()
                .filter_map(|tag| tag.strip_prefix("hub:"))
                .eq([self.config.hub.as_str()]);
            let control = snapshot.tags.iter().any(|tag| tag.starts_with("uah2:"));
            if control && repository.url == self.config.repositories.shared.url {
                crate::migration::read_control(self.config, self, snapshot)?;
            } else if !legacy_hub || control {
                return Err(Error::new("Snapshot hub does not match configuration"));
            }
        }
        Ok(snapshots)
    }

    pub(crate) fn snapshot(&self, repository: &Repository, id: &str) -> Result<Snapshot> {
        if !snapshot_id(id) {
            return Err(Error::new("Expected a full snapshot identifier"));
        }
        let bytes = self.run(
            repository,
            &[
                "cat".into(),
                "snapshot".into(),
                id.into(),
                "--no-lock".into(),
            ],
            None,
        )?;
        let mut snapshot: Snapshot = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("Invalid restic snapshot object"))?;
        if !snapshot.id.is_empty() && !snapshot.id.eq_ignore_ascii_case(id) {
            return Err(Error::new(
                "Restic snapshot identity does not match request",
            ));
        }
        snapshot.id = id.to_owned();
        Ok(snapshot)
    }

    pub(crate) fn format_fence(
        &self,
        config_path: &Path,
        expected_source: Option<&str>,
    ) -> Result<String> {
        let executable = std::env::current_exe()
            .map_err(|_| Error::new("Cannot locate the migration helper executable"))?;
        let mut arguments: Vec<OsString> = vec![
            "backup".into(),
            "--json".into(),
            "--host".into(),
            self.config.id().into(),
            "--stdin-filename".into(),
            crate::migration::FENCE_FILE.into(),
        ];
        for tag in crate::migration::control_tags(&self.config.hub) {
            arguments.extend(["--tag".into(), tag.into()]);
        }
        arguments.extend([
            "--stdin-from-command".into(),
            "--".into(),
            executable.into_os_string(),
            crate::migration::HELPER.into(),
            config_path.as_os_str().to_owned(),
            expected_source.unwrap_or("empty").into(),
        ]);
        let bytes = self.run(&self.config.repositories.shared, &arguments, None)?;
        backup_snapshot_id(&bytes)
    }

    pub fn backup_uncommitted(
        &self,
        repository: &Repository,
        export: &Export,
        kind: &str,
    ) -> Result<String> {
        if !crate::config::identity(kind) || matches!(kind, "shared" | "bot") {
            return Err(Error::new(
                "Immutable state requires a distinct snapshot kind",
            ));
        }
        let existing = self.snapshots(repository)?;
        let run_tag = format!(
            "run:{}",
            export
                .tree
                .path()
                .file_name()
                .expect("temporary export")
                .to_string_lossy()
        );
        if existing
            .iter()
            .any(|snapshot| snapshot.tags.contains(&run_tag))
        {
            return Err(Error::new(
                "Backup run already exists; no write was replayed",
            ));
        }
        let tags = [
            "utsurigi-agent-hub".to_owned(),
            format!("hub:{}", self.config.hub),
            format!("bot:{}", self.config.id()),
            format!("runtime:{}", self.config.runtime_id()),
            format!("kind:{kind}"),
            run_tag,
        ];
        let mut arguments: Vec<OsString> = vec![
            "backup".into(),
            "--json".into(),
            "--host".into(),
            self.config.id().into(),
            "--read-concurrency".into(),
            self.config.sync.read_concurrency.to_string().into(),
        ];
        for tag in &tags {
            arguments.extend(["--tag".into(), tag.into()]);
        }
        arguments.push(".".into());
        let bytes = self.run(repository, &arguments, Some(export.tree.path()))?;
        let id = backup_snapshot_id(&bytes)?;
        if existing
            .iter()
            .any(|snapshot| snapshot.id.eq_ignore_ascii_case(&id))
        {
            return Err(Error::new("Backup summary identifies an existing snapshot"));
        }
        let snapshot = self.snapshot(repository, &id)?;
        if snapshot.hostname != self.config.id()
            || snapshot.tags.len() != tags.len()
            || tags.iter().any(|tag| !snapshot.tags.contains(tag))
        {
            return Err(Error::new("Immutable backup does not match this write"));
        }
        Ok(id)
    }

    pub fn initialize(&self, repository: &Repository) -> Result<()> {
        match self.snapshots(repository) {
            Ok(_) => Ok(()),
            Err(error) if error.code == 10 => {
                self.run(repository, &["init".into()], None)?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub fn repository_id(&self, repository: &Repository) -> Result<String> {
        let bytes = self.run(
            repository,
            &["cat".into(), "config".into(), "--no-lock".into()],
            None,
        )?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("Invalid restic repository identity"))?;
        value
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| snapshot_id(s))
            .map(str::to_owned)
            .ok_or_else(|| Error::new("Invalid restic repository identity"))
    }

    pub fn backup(&self, repository: &Repository, export: &Export, kind: &str) -> Result<()> {
        // Reconcile existing snapshots first; never retry an ambiguous whole write.
        let existing = self.snapshots(repository)?;
        let run_tag = format!(
            "run:{}",
            export
                .tree
                .path()
                .file_name()
                .expect("temporary directory")
                .to_string_lossy()
        );
        if existing
            .iter()
            .any(|snapshot| snapshot.tags.contains(&run_tag))
        {
            return Err(Error::new(
                "Backup run already exists; no write was replayed",
            ));
        }
        let mut arguments: Vec<OsString> = vec![
            "backup".into(),
            "--json".into(),
            "--host".into(),
            self.config.id().into(),
            "--read-concurrency".into(),
            self.config.sync.read_concurrency.to_string().into(),
        ];
        for tag in [
            "utsurigi-agent-hub".to_string(),
            format!("hub:{}", self.config.hub),
            format!("bot:{}", self.config.id()),
            format!("runtime:{}", self.config.runtime_id()),
            format!("kind:{kind}"),
            run_tag.clone(),
        ] {
            arguments.push("--tag".into());
            arguments.push(tag.into());
        }
        arguments.push(".".into());
        let output = self.run(repository, &arguments, Some(export.tree.path()))?;
        let id = backup_snapshot_id(&output)?;
        if existing
            .iter()
            .any(|snapshot| snapshot.id.eq_ignore_ascii_case(&id))
        {
            return Err(Error::new(
                "Backup summary identifies an existing snapshot; no completion marker was added",
            ));
        }
        // Explicit IDs bypass restic's tag filters. Validate the immutable object before tagging.
        let bytes = self.run(
            repository,
            &[
                "cat".into(),
                "snapshot".into(),
                id.clone().into(),
                "--no-lock".into(),
            ],
            None,
        )?;
        let snapshot: Snapshot = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new("Invalid restic backup snapshot; no completion marker was added")
        })?;
        let expected = [
            ("hub:", self.config.hub.as_str()),
            ("bot:", self.config.id()),
            ("runtime:", self.config.runtime_id()),
            ("kind:", kind),
            ("run:", run_tag.strip_prefix("run:").expect("run tag")),
        ];
        let matches_write = |snapshot: &Snapshot| {
            snapshot.owned_by(self.config.id(), kind)
                && expected.iter().all(|(prefix, value)| {
                    snapshot
                        .tags
                        .iter()
                        .filter_map(|tag| tag.strip_prefix(prefix))
                        .eq([*value])
                })
        };
        if !matches_write(&snapshot)
            || snapshot
                .tags
                .iter()
                .any(|tag| tag.starts_with("completed:"))
        {
            return Err(Error::new(
                "Backup snapshot does not match this write; no completion marker was added",
            ));
        }
        let completed_at = chrono::Utc::now().timestamp();
        self.run(
            repository,
            &[
                "tag".into(),
                "--add".into(),
                format!("completed:{completed_at}").into(),
                id.into(),
            ],
            None,
        )?;
        let mut matching = self
            .snapshots(repository)?
            .into_iter()
            .filter(|snapshot| snapshot.tags.contains(&run_tag));
        let completed = matching.next().filter(|snapshot| {
            matches_write(snapshot) && snapshot.completed_at() == Some(completed_at)
        });
        if completed.is_none() || matching.next().is_some() {
            return Err(Error::new("Completion marker could not be reconciled"));
        }
        Ok(())
    }

    pub fn restore(&self, repository: &Repository, id: &str, destination: &Path) -> Result<()> {
        if !snapshot_id(id) {
            return Err(Error::new("Restore requires an explicit full snapshot ID"));
        }
        self.run(
            repository,
            &[
                "restore".into(),
                id.into(),
                "--target".into(),
                destination.as_os_str().to_owned(),
                "--verify".into(),
            ],
            None,
        )?;
        validate_import(destination)
    }
}

pub fn snapshot_id(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn latest<'a>(snapshots: &'a [Snapshot], kind: &str) -> Option<&'a Snapshot> {
    snapshots
        .iter()
        .filter(|s| s.kind(kind) && s.completed_at().is_some())
        .max_by_key(|s| {
            (
                s.completed_at(),
                chrono::DateTime::parse_from_rfc3339(&s.time).ok(),
                &s.id,
            )
        })
}

fn download(url: &str, destination: &Path, limit: u64) -> Result<()> {
    let mut command = Command::new("curl");
    safe_environment(&mut command);
    let output = command
        .args([
            "--disable",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--max-filesize",
        ])
        .arg(limit.to_string())
        .arg("--output")
        .arg(destination)
        .arg(url)
        .output()
        .map_err(|_| Error::new("Restic acquisition requires curl with HTTPS support"))?;
    if !output.status.success() {
        return Err(Error::new(
            "Official restic download failed; no executable was installed",
        ));
    }
    Ok(())
}

fn acquire_official(target: &Path, version: &str) -> Result<()> {
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        _ => {
            return Err(Error::new(
                "Configure a compatible restic executable for this architecture",
            ));
        }
    };
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "freebsd" => "freebsd",
        _ => {
            return Err(Error::new(
                "Configure a compatible restic executable for this operating system",
            ));
        }
    };
    let parent = target
        .parent()
        .ok_or_else(|| Error::new("Invalid restic executable path"))?;
    crate::config::no_symlinks(parent)?;
    if !parent.exists() {
        private_dir(parent)?;
    }
    let asset = format!("restic_{version}_{os}_{architecture}.bz2");
    let base = format!("https://github.com/restic/restic/releases/download/v{version}");
    let checksums = tempfile::NamedTempFile::new_in(parent)?;
    let archive = tempfile::NamedTempFile::new_in(parent)?;
    download(&format!("{base}/SHA256SUMS"), checksums.path(), 1024 * 1024)?;
    let sums = fs::read_to_string(checksums.path())?;
    let expected = sums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            (fields.next()? == asset).then_some(digest)
        })
        .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| Error::new("Official checksum does not cover this restic asset"))?;
    download(
        &format!("{base}/{asset}"),
        archive.path(),
        128 * 1024 * 1024,
    )?;
    let mut hash = Sha256::new();
    let mut file = File::open(archive.path())?;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase() {
        return Err(Error::new(
            "Official restic checksum mismatch; no executable was installed",
        ));
    }
    let mut executable = tempfile::NamedTempFile::new_in(parent)?;
    private_file(executable.as_file())?;
    let mut decoder =
        bzip2::read::BzDecoder::new(File::open(archive.path())?).take(256 * 1024 * 1024 + 1);
    let size = std::io::copy(&mut decoder, &mut executable)?;
    if size > 256 * 1024 * 1024 {
        return Err(Error::new("Restic executable exceeds acquisition limit"));
    }
    executable.flush()?;
    executable.as_file().sync_all()?;
    let path = executable.into_temp_path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    if !compatible(&path, version) {
        return Err(Error::new(
            "Downloaded restic did not match its pinned version",
        ));
    }
    path.persist_noclobber(target).map_err(|_| {
        Error::new("Cannot install verified restic without replacing an existing file")
    })?;
    Ok(())
}
