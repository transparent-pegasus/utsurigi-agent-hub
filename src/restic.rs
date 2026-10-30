use crate::config::{Config, Repository};
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
}

#[derive(Deserialize)]
pub struct Snapshot {
    pub id: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub tags: Vec<String>,
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
    for name in ["PATH", "SYSTEMROOT", "WINDIR"] {
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
        })
    }

    fn output(
        &self,
        repository: &Repository,
        arguments: &[OsString],
        cwd: Option<&Path>,
    ) -> Result<Output> {
        let mut command = Command::new(&self.executable);
        safe_environment(&mut command);
        command
            .env("RESTIC_PASSWORD", password(repository)?)
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
            return Err(Error {
                message: format!("restic operation failed (exit {code}); no write was replayed"),
                code,
            });
        }
        Ok(output.stdout)
    }

    pub fn snapshots(&self, repository: &Repository) -> Result<Vec<Snapshot>> {
        let bytes = self.run(repository, &["snapshots".into(), "--json".into()], None)?;
        let snapshots: Vec<Snapshot> = serde_json::from_slice(&bytes)
            .map_err(|_| Error::new("Invalid restic snapshot response"))?;
        if snapshots.iter().any(|s| !snapshot_id(&s.id)) {
            return Err(Error::new("Invalid restic snapshot identifier"));
        }
        if snapshots.iter().any(|snapshot| {
            !snapshot
                .tags
                .iter()
                .filter_map(|tag| tag.strip_prefix("hub:"))
                .eq([self.config.hub.as_str()])
        }) {
            return Err(Error::new("Snapshot hub does not match configuration"));
        }
        Ok(snapshots)
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
        let bytes = self.run(repository, &["cat".into(), "config".into()], None)?;
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
        self.snapshots(repository)?;
        let run_tag = format!(
            "run:{}",
            export
                .tree
                .path()
                .file_name()
                .expect("temporary directory")
                .to_string_lossy()
        );
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
        self.run(repository, &arguments, Some(export.tree.path()))?;
        let snapshots = self.snapshots(repository)?;
        let matching: Vec<_> = snapshots
            .iter()
            .filter(|s| s.tags.contains(&run_tag) && s.owned_by(self.config.id(), kind))
            .collect();
        if matching.len() != 1 {
            return Err(Error::new(
                "Completed write could not be reconciled; no completion marker was added",
            ));
        }
        let id = &matching[0].id;
        self.run(
            repository,
            &[
                "tag".into(),
                "--add".into(),
                format!("completed:{}", chrono::Utc::now().timestamp()).into(),
                id.into(),
            ],
            None,
        )?;
        self.snapshots(repository)?
            .into_iter()
            .find(|s| s.tags.contains(&run_tag) && s.completed_at().is_some())
            .map(|_| ())
            .ok_or_else(|| Error::new("Completion marker could not be reconciled"))
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
