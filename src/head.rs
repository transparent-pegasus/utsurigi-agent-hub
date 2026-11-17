use crate::config::{CHILD_ENV, Config, identity, no_symlinks};
use crate::error::{Error, Result};
use crate::state::private_file;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HEAD_NAME: &str = "uah-head.json";
const SUBJECT: &str = "Shared head";
const BODY_LIMIT: u64 = 16 * 1024;
const HEADER_LIMIT: u64 = 64 * 1024;
const TOKEN_LIMIT: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Head {
    pub version: u32,
    pub hub: String,
    pub revision: u64,
    pub snapshot: String,
}

#[derive(Clone, Debug)]
pub struct ObservedHead {
    pub head: Head,
    pub token: String,
}

pub struct HeadStore {
    hub: String,
    backend: Backend,
    timeout: Duration,
}

pub(crate) enum Backend {
    Local(PathBuf),
    R2(String),
}

impl HeadStore {
    pub fn new(config: &Config) -> Result<Self> {
        if !identity(&config.hub) {
            return Err(Error::new("Invalid head hub identity"));
        }
        let repository = config
            .repositories
            .shared
            .url
            .as_deref()
            .ok_or_else(|| Error::new("Missing shared repository URL for head"))?;
        config.validate_url(repository)?;
        let backend = if config.storage.is_local_backed() {
            Backend::Local(PathBuf::from(repository))
        } else {
            Backend::R2(format!("{}/{HEAD_NAME}", r2_base(repository, SUBJECT)?))
        };
        Ok(Self {
            hub: config.hub.clone(),
            backend,
            timeout: Duration::from_secs(config.sync.stuck_request_timeout_seconds.clamp(1, 120)),
        })
    }

    pub fn read(&self) -> Result<Option<ObservedHead>> {
        match &self.backend {
            Backend::Local(root) => self.read_local(root),
            Backend::R2(url) => {
                let response = request("GET", url, &[], None, BODY_LIMIT, self.timeout)?
                    .ok_or_else(|| {
                        Error::new("Shared head GET transport failed; retry the read")
                    })?;
                match response.status {
                    404 => Ok(None),
                    200 => {
                        let token = response
                            .etag
                            .filter(|token| strong_etag(token))
                            .ok_or_else(|| Error::new("Invalid or missing shared head ETag"))?;
                        Ok(Some(ObservedHead {
                            head: self.decode(&response.body)?,
                            token,
                        }))
                    }
                    status => Err(http_error("GET", status)),
                }
            }
        }
    }

    pub fn compare_exchange(&self, expected: Option<&ObservedHead>, next: &Head) -> Result<bool> {
        self.validate(next)?;
        if let Some(expected) = expected {
            self.validate(&expected.head)?;
            if expected.head.revision.checked_add(1) != Some(next.revision) {
                return Err(Error::new("Shared head revision must increase by one"));
            }
        } else if next.revision != 1 {
            return Err(Error::new("Initial shared head revision must be one"));
        }
        let body = serde_json::to_vec(next).map_err(|_| Error::new("Cannot encode shared head"))?;
        match &self.backend {
            Backend::Local(root) => {
                let _lock = local_lock(
                    root,
                    "uah-head.lock",
                    SUBJECT,
                    self.timeout.min(Duration::from_secs(30)),
                )?;
                let current = self.read_local(root)?;
                let matches = match (expected, current.as_ref()) {
                    (None, None) => true,
                    (Some(expected), Some(current)) => {
                        expected.token == current.token && expected.head == current.head
                    }
                    _ => false,
                };
                if !matches {
                    return Ok(false);
                }
                replace_private(root, &root.join(HEAD_NAME), &body, SUBJECT)?;
                Ok(true)
            }
            Backend::R2(url) => {
                let condition = if let Some(expected) = expected {
                    if !strong_etag(&expected.token) {
                        return Err(Error::new("Invalid observed shared head ETag"));
                    }
                    format!("If-Match: {}", expected.token)
                } else {
                    "If-None-Match: *".to_owned()
                };
                match request(
                    "PUT",
                    url,
                    &["Content-Type: application/json", &condition],
                    Some(&body),
                    BODY_LIMIT,
                    self.timeout,
                )? {
                    Some(response) => match response.status {
                        200 | 201 | 204 => Ok(true),
                        412 => Ok(false),
                        status => Err(http_error("PUT", status)),
                    },
                    // The server may have accepted the conditional write before the
                    // connection failed. Never replay it or use an unconditional PUT.
                    None => match self.read()? {
                        Some(current) => Ok(current.head == *next),
                        None => Ok(false),
                    },
                }
            }
        }
    }

    fn validate(&self, head: &Head) -> Result<()> {
        if head.version != 2
            || head.hub != self.hub
            || !identity(&head.hub)
            || head.revision == 0
            || head.snapshot.len() != 64
            || !head
                .snapshot
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(Error::new(
                "Invalid shared head version, hub, revision, or snapshot ID",
            ));
        }
        Ok(())
    }

    fn decode(&self, bytes: &[u8]) -> Result<Head> {
        let head: Head =
            serde_json::from_slice(bytes).map_err(|_| Error::new("Invalid shared head JSON"))?;
        self.validate(&head)?;
        Ok(head)
    }

    fn read_local(&self, root: &Path) -> Result<Option<ObservedHead>> {
        let Some((bytes, token)) = read_private(root, &root.join(HEAD_NAME), BODY_LIMIT, SUBJECT)?
        else {
            return Ok(None);
        };
        Ok(Some(ObservedHead {
            head: self.decode(&bytes)?,
            token,
        }))
    }
}

fn local_root(root: &Path, subject: &str) -> Result<()> {
    no_symlinks(root)?;
    if !fs::metadata(root)?.is_dir() {
        return Err(Error::new(format!(
            "{subject} repository must be a directory"
        )));
    }
    Ok(())
}

pub(crate) fn read_private(
    root: &Path,
    path: &Path,
    limit: u64,
    subject: &str,
) -> Result<Option<(Vec<u8>, String)>> {
    local_root(root, subject)?;
    no_symlinks(path)?;
    let mut options = local_options();
    options.read(true);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    regular_file(&file, subject)?;
    let bytes = read_bounded(&file, limit, subject)?;
    // This token identifies the exact bytes observed, including formatting.
    let token = format!("{:x}", Sha256::digest(&bytes));
    Ok(Some((bytes, token)))
}

pub(crate) fn replace_private(
    directory: &Path,
    path: &Path,
    bytes: &[u8],
    subject: &str,
) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    private_file(temporary.as_file(), temporary.path())?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    no_symlinks(path)?;
    temporary.persist(path).map_err(|_| {
        Error::new(format!(
            "Cannot atomically replace {}",
            subject.to_lowercase()
        ))
    })?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn local_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // NONBLOCK prevents a malicious FIFO from blocking before its type check.
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn regular_file(file: &File, subject: &str) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::new(format!(
            "{subject} and lock must be regular files"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A reader may still hold the old inode after an atomic replacement.
        if metadata.nlink() > 1 {
            return Err(Error::new(format!(
                "{subject} and lock must not have hard links"
            )));
        }
    }
    Ok(())
}

pub(crate) fn local_lock(
    root: &Path,
    name: &str,
    subject: &str,
    timeout: Duration,
) -> Result<File> {
    local_root(root, subject)?;
    let path = root.join(name);
    no_symlinks(&path)?;
    let file = local_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    regular_file(&file, subject)?;
    private_file(&file, &path)?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(Error::new(format!(
                    "Timed out waiting for {} CAS lock; retry",
                    subject.to_lowercase()
                )));
            }
            Err(TryLockError::Error(_)) => {
                return Err(Error::new(format!(
                    "Cannot acquire {} CAS lock",
                    subject.to_lowercase()
                )));
            }
        }
    }
}

fn read_bounded(file: &File, limit: u64, subject: &str) -> Result<Vec<u8>> {
    let exceeded = || Error::new(format!("{subject} response exceeds its size limit"));
    if file.metadata()?.len() > limit {
        return Err(exceeded());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(exceeded());
    }
    Ok(bytes)
}

pub(crate) fn r2_base(repository: &str, subject: &str) -> Result<String> {
    let invalid = || Error::new(format!("Invalid HTTPS {} endpoint", subject.to_lowercase()));
    let endpoint = repository.strip_prefix("s3:https://").ok_or_else(invalid)?;
    let (authority, path) = endpoint.split_once('/').ok_or_else(invalid)?;
    if authority.len() > 512 {
        return Err(invalid());
    }
    let port = if let Some(ipv6) = authority.strip_prefix('[') {
        let (address, rest) = ipv6.split_once(']').ok_or_else(invalid)?;
        address
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| invalid())?;
        if rest.is_empty() {
            None
        } else {
            Some(rest.strip_prefix(':').ok_or_else(invalid)?)
        }
    } else {
        let (host, port) = authority
            .split_once(':')
            .map_or((authority, None), |(host, port)| (host, Some(port)));
        if host.is_empty()
            || host.split('.').any(|label| {
                label.is_empty()
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            })
        {
            return Err(invalid());
        }
        port
    };
    if port.is_some_and(|port| {
        !port.bytes().all(|c| c.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|value| value > 0)
    }) {
        return Err(invalid());
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() || path.len() > 4096 {
        return Err(invalid());
    }
    for segment in path.split('/') {
        let mut decoded = Vec::new();
        let mut bytes = segment.bytes();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'%' {
                let high = bytes.next().and_then(|c| (c as char).to_digit(16));
                let low = bytes.next().and_then(|c| (c as char).to_digit(16));
                match (high, low) {
                    (Some(high), Some(low)) => (high * 16 + low) as u8,
                    _ => return Err(invalid()),
                }
            } else if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:".contains(&byte) {
                byte
            } else {
                return Err(invalid());
            };
            if byte.is_ascii_control() || matches!(byte, b'/' | b'\\') {
                return Err(invalid());
            }
            decoded.push(byte);
        }
        if decoded.is_empty() || decoded == b"." || decoded == b".." {
            return Err(invalid());
        }
    }
    Ok(format!("https://{authority}/{path}"))
}

pub(crate) fn strong_etag(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && bytes.len() <= TOKEN_LIMIT
        && bytes.first() == Some(&b'"')
        && bytes.last() == Some(&b'"')
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|&c| c == 0x21 || (0x23..=0x7e).contains(&c) || c >= 0x80)
}

fn http_error(method: &str, status: u16) -> Error {
    Error::new(format!(
        "Shared head {method} failed (HTTP {status}); check backend access and retry"
    ))
}

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) etag: Option<String>,
    pub(crate) body: Vec<u8>,
}

fn option(config: &mut String, name: &str, value: &str) -> Result<()> {
    if value.len() > 16 * 1024 || value.chars().any(char::is_control) {
        return Err(Error::new("Invalid R2 transport option"));
    }
    config.push_str(name);
    config.push_str(" = \"");
    for c in value.chars() {
        if matches!(c, '\\' | '"') {
            config.push('\\');
        }
        config.push(c);
    }
    config.push_str("\"\n");
    Ok(())
}

fn credential(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .ok_or_else(|| {
            Error::new("R2 credentials were not delivered in a supported form by the host")
        })
}

// None means a request was started but no trustworthy complete response arrived.
// Callers must never replay a PUT; they reconcile it using an exact read.
// `limit` bounds both the uploaded and the response body.
pub(crate) fn request(
    method: &str,
    url: &str,
    headers: &[&str],
    body: Option<&[u8]>,
    limit: u64,
    timeout: Duration,
) -> Result<Option<Response>> {
    let directory = tempfile::Builder::new().prefix("uah-r2-").tempdir()?;
    no_symlinks(directory.path())?;
    let mut response_body = tempfile::NamedTempFile::new_in(directory.path())?;
    let mut response_headers = tempfile::NamedTempFile::new_in(directory.path())?;
    let mut status = tempfile::NamedTempFile::new_in(directory.path())?;
    let mut upload = tempfile::NamedTempFile::new_in(directory.path())?;
    for file in [&response_body, &response_headers, &status, &upload] {
        private_file(file.as_file(), file.path())?;
    }
    let mut config = String::from(
        "silent\ngloboff\nsuppress-connect-headers\nproto = \"=https\"\nproto-redir = \"=https\"\nconnect-timeout = \"15\"\n",
    );
    option(&mut config, "max-time", &timeout.as_secs().to_string())?;
    option(&mut config, "max-filesize", &limit.to_string())?;
    option(&mut config, "url", url)?;
    option(&mut config, "aws-sigv4", "aws:amz:auto:s3")?;
    let access = credential("AWS_ACCESS_KEY_ID")?;
    if access.contains(':') {
        return Err(Error::new("Invalid R2 access key format"));
    }
    option(
        &mut config,
        "user",
        &format!("{access}:{}", credential("AWS_SECRET_ACCESS_KEY")?),
    )?;
    if let Some(token) = std::env::var_os("AWS_SESSION_TOKEN").filter(|value| !value.is_empty()) {
        let token = token
            .into_string()
            .map_err(|_| Error::new("Invalid R2 session token"))?;
        option(
            &mut config,
            "header",
            &format!("X-Amz-Security-Token: {token}"),
        )?;
    }
    for (name, target) in [
        ("RESTIC_CACERT", "cacert"),
        ("RESTIC_TLS_CLIENT_CERT", "cert"),
    ] {
        if let Some(path) = std::env::var_os(name) {
            let path = path
                .into_string()
                .map_err(|_| Error::new("Invalid R2 TLS option"))?;
            option(&mut config, target, &path)?;
        }
    }
    for (name, file) in [
        ("output", &response_body),
        ("dump-header", &response_headers),
    ] {
        option(
            &mut config,
            name,
            file.path()
                .to_str()
                .ok_or_else(|| Error::new("Invalid R2 temporary path"))?,
        )?;
    }
    option(&mut config, "write-out", "%{http_code}")?;
    option(&mut config, "request", method)?;
    if method == "PUT"
        && !headers
            .iter()
            .any(|header| header.starts_with("If-Match: ") || header.starts_with("If-None-Match: "))
    {
        return Err(Error::new("Missing R2 PUT CAS condition"));
    }
    for header in headers {
        option(&mut config, "header", header)?;
    }
    if let Some(body) = body {
        if body.len() as u64 > limit {
            return Err(Error::new("R2 request exceeds its size limit"));
        }
        upload.write_all(body)?;
        upload.flush()?;
        option(
            &mut config,
            "data-binary",
            &format!(
                "@{}",
                upload
                    .path()
                    .to_str()
                    .ok_or_else(|| Error::new("Invalid R2 temporary path"))?
            ),
        )?;
    }
    if config.len() > 32 * 1024 {
        return Err(Error::new(
            "R2 transport configuration exceeds its size limit",
        ));
    }
    let mut command = Command::new("curl");
    command
        .args(["-q", "--config", "-"])
        .env_clear()
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("TZ", "UTC")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(status.as_file().try_clone()?))
        .stderr(Stdio::null());
    for name in CHILD_ENV {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    bound_child_files(&mut command, HEADER_LIMIT.max(limit))?;
    let mut child = command
        .spawn()
        .map_err(|_| Error::new("R2 transport requires curl with HTTPS and AWS SigV4 support"))?;
    let input = child.stdin.take().expect("piped curl stdin");
    if nonblocking(&input).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::new("Cannot prepare bounded R2 transport"));
    }
    let started = Instant::now();
    let mut remaining = config.as_bytes();
    let mut input = Some(input);
    let completed = loop {
        if let Some(writer) = input.as_mut() {
            match writer.write(remaining) {
                Ok(count) => {
                    remaining = &remaining[count..];
                    if remaining.is_empty() {
                        input = None;
                        // Close the pipe so curl sees EOF after its config.
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }
            }
        }
        if input.is_none() {
            break None;
        }
        match child.try_wait() {
            Ok(Some(exit)) => break Some(exit.success()),
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
        }
    };
    drop(input);
    finish_request(
        child,
        completed,
        started + timeout,
        limit,
        &mut response_body,
        &mut response_headers,
        &mut status,
    )
}

fn finish_request(
    mut child: std::process::Child,
    completed: Option<bool>,
    deadline: Instant,
    limit: u64,
    body: &mut tempfile::NamedTempFile,
    headers: &mut tempfile::NamedTempFile,
    status: &mut tempfile::NamedTempFile,
) -> Result<Option<Response>> {
    let success = if let Some(success) = completed {
        success
    } else {
        loop {
            match child.try_wait() {
                Ok(Some(exit)) => break exit.success(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }
            }
        }
    };
    if !success {
        return Ok(None);
    }
    let mut parse = || -> Result<Response> {
        for file in [&mut *body, &mut *headers, &mut *status] {
            file.seek(SeekFrom::Start(0))?;
        }
        let status_bytes = read_bounded(status.as_file(), 3, "R2")?;
        let status_text =
            std::str::from_utf8(&status_bytes).map_err(|_| Error::new("Invalid R2 HTTP status"))?;
        if status_bytes.len() != 3 || !status_bytes.iter().all(u8::is_ascii_digit) {
            return Err(Error::new("Invalid R2 HTTP status"));
        }
        let status = status_text
            .parse::<u16>()
            .map_err(|_| Error::new("Invalid R2 HTTP status"))?;
        let etag = parse_headers(
            &read_bounded(headers.as_file(), HEADER_LIMIT, "R2")?,
            status,
        )?;
        Ok(Response {
            status,
            etag,
            body: read_bounded(body.as_file(), limit, "R2")?,
        })
    };
    Ok(parse().ok())
}

fn parse_headers(bytes: &[u8], status: u16) -> Result<Option<String>> {
    let invalid = || Error::new("Invalid R2 HTTP headers");
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let mut final_status = None;
    let mut etag = None;
    let mut ended = true;
    for line in text.split_terminator('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(invalid());
        }
        if line.is_empty() {
            ended = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("HTTP/") {
            if !ended || final_status.is_some_and(|status| status >= 200) {
                return Err(invalid());
            }
            let mut fields = rest.splitn(3, ' ');
            if !matches!(fields.next(), Some("1.0" | "1.1" | "2" | "3")) {
                return Err(invalid());
            }
            let code = fields.next().ok_or_else(invalid)?;
            if code.len() != 3 || !code.bytes().all(|c| c.is_ascii_digit()) {
                return Err(invalid());
            }
            final_status = Some(code.parse::<u16>().map_err(|_| invalid())?);
            etag = None;
            ended = false;
            continue;
        }
        if ended || final_status.is_none() || line.starts_with([' ', '\t']) {
            return Err(invalid());
        }
        let (name, value) = line.split_once(':').ok_or_else(invalid)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
        {
            return Err(invalid());
        }
        if name.eq_ignore_ascii_case("etag") {
            if etag.is_some() {
                return Err(invalid());
            }
            let value = value.trim_matches([' ', '\t']);
            if !strong_etag(value) {
                return Err(invalid());
            }
            etag = Some(value.to_owned());
        }
    }
    if !ended || final_status != Some(status) || !(100..=599).contains(&status) {
        return Err(invalid());
    }
    Ok(etag)
}

#[cfg(unix)]
fn nonblocking(input: &std::process::ChildStdin) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: input owns a live pipe descriptor; fcntl only changes its status flags.
    unsafe {
        let flags = libc::fcntl(input.as_raw_fd(), libc::F_GETFL);
        if flags == -1
            || libc::fcntl(input.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn bound_child_files(command: &mut Command, file_limit: u64) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // SAFETY: the child hook only calls async-signal-safe setrlimit and constructs
    // an OS error on failure. It does not touch parent process limits.
    unsafe {
        command.pre_exec(move || {
            let no_core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &no_core) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let limit = libc::rlimit {
                rlim_cur: file_limit as libc::rlim_t,
                rlim_max: file_limit as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn bound_child_files(_: &mut Command, _: u64) -> Result<()> {
    Err(Error::new(
        "Bounded R2 transport requires a supported Unix host",
    ))
}

#[cfg(not(unix))]
fn nonblocking(_: &std::process::ChildStdin) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unsupported pipe",
    ))
}

#[cfg(test)]
#[path = "head_tests.rs"]
mod tests;
