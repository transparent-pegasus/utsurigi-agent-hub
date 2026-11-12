//! Small-object transport for one repository URL: R2 over curl SigV4, or a local directory.

use crate::config::{Config, no_symlinks};
use crate::error::{Error, Result};
use crate::head::{
    Backend, local_lock, r2_base, read_private, replace_private, request, strong_etag,
};
use crate::state::private_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MAX_OBJECT_BYTES: u64 = 1 << 20;
const SUBJECT: &str = "Live object";
const LOCK_NAME: &str = "uah-objects.lock";

#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    Missing,
    NotModified,
    Found { body: Vec<u8>, token: String },
}

pub enum Condition<'a> {
    Create,
    Match(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    Yes,
    Conflict,
    Uncertain,
}

pub struct ObjectStore {
    backend: Backend,
    timeout: Duration,
}

impl ObjectStore {
    pub fn new(config: &Config, repository: &str) -> Result<Self> {
        config.validate_url(repository)?;
        let backend = if config.storage.is_local_backed() {
            Backend::Local(PathBuf::from(repository))
        } else {
            Backend::R2(r2_base(repository, SUBJECT)?)
        };
        Ok(Self {
            backend,
            timeout: Duration::from_secs(config.sync.stuck_request_timeout_seconds.clamp(1, 120)),
        })
    }

    pub fn get(&self, key: &str, if_none_match: Option<&str>, limit: u64) -> Result<Fetched> {
        valid_key(key)?;
        if limit > MAX_OBJECT_BYTES {
            return Err(Error::new("Live object size limit must not exceed 1 MiB"));
        }
        match &self.backend {
            Backend::Local(root) => {
                Ok(match read_private(root, &root.join(key), limit, SUBJECT)? {
                    None => Fetched::Missing,
                    Some((_, token)) if if_none_match == Some(token.as_str()) => {
                        Fetched::NotModified
                    }
                    Some((body, token)) => Fetched::Found { body, token },
                })
            }
            Backend::R2(base) => {
                let condition = match if_none_match {
                    Some(token) => Some(format!("If-None-Match: {}", etag(token)?)),
                    None => None,
                };
                let headers = condition.as_deref();
                let url = format!("{base}/{key}");
                let response = request("GET", &url, headers.as_slice(), None, limit, self.timeout)?
                    .ok_or_else(|| {
                        Error::new("Live object GET transport failed; retry the read")
                    })?;
                match response.status {
                    200 => Ok(Fetched::Found {
                        token: response
                            .etag
                            .ok_or_else(|| Error::new("Invalid or missing live object ETag"))?,
                        body: response.body,
                    }),
                    // A 304 to an unconditional GET would hide a never-fetched object.
                    304 if if_none_match.is_some() => Ok(Fetched::NotModified),
                    404 => Ok(Fetched::Missing),
                    status => Err(http_error("GET", status)),
                }
            }
        }
    }

    pub fn put(&self, key: &str, body: &[u8], condition: Condition<'_>) -> Result<Written> {
        valid_key(key)?;
        if body.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::new("Live object request exceeds its size limit"));
        }
        match &self.backend {
            Backend::Local(root) => {
                let _lock = self.lock(root)?;
                let path = root.join(key);
                let current = read_private(root, &path, MAX_OBJECT_BYTES, SUBJECT)?;
                let accepted = match (condition, current) {
                    (Condition::Create, None) => true,
                    (Condition::Match(expected), Some((_, token))) => expected == token,
                    _ => false,
                };
                if !accepted {
                    return Ok(Written::Conflict);
                }
                let parent = path.parent().expect("a joined key has a parent");
                // Never re-permission the existing repository directory itself.
                if parent != root.as_path() {
                    private_dir(parent)?;
                }
                replace_private(parent, &path, body, SUBJECT)?;
                Ok(Written::Yes)
            }
            Backend::R2(base) => {
                let condition = match condition {
                    Condition::Create => "If-None-Match: *".to_owned(),
                    Condition::Match(token) => format!("If-Match: {}", etag(token)?),
                };
                let headers = ["Content-Type: application/octet-stream", condition.as_str()];
                let url = format!("{base}/{key}");
                match request(
                    "PUT",
                    &url,
                    &headers,
                    Some(body),
                    MAX_OBJECT_BYTES,
                    self.timeout,
                )? {
                    Some(response) => match response.status {
                        200 | 201 | 204 => Ok(Written::Yes),
                        412 => Ok(Written::Conflict),
                        status => Err(http_error("PUT", status)),
                    },
                    // The server may have accepted it; the caller reconciles by reading.
                    None => Ok(Written::Uncertain),
                }
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        valid_key(key)?;
        match &self.backend {
            Backend::Local(root) => {
                let _lock = self.lock(root)?;
                let path = root.join(key);
                no_symlinks(&path)?;
                match fs::remove_file(&path) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
                    _ => Ok(()),
                }
            }
            Backend::R2(base) => {
                let url = format!("{base}/{key}");
                match request("DELETE", &url, &[], None, MAX_OBJECT_BYTES, self.timeout)? {
                    Some(response) if matches!(response.status, 200 | 204 | 404) => Ok(()),
                    Some(response) => Err(http_error("DELETE", response.status)),
                    None => Err(Error::new("Live object DELETE transport failed; retry")),
                }
            }
        }
    }

    fn lock(&self, root: &Path) -> Result<fs::File> {
        local_lock(
            root,
            LOCK_NAME,
            SUBJECT,
            self.timeout.min(Duration::from_secs(30)),
        )
    }
}

fn valid_key(key: &str) -> Result<()> {
    let segment = |segment: &str| {
        !matches!(segment, "" | "." | "..")
            && segment
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    };
    if key.len() > 512 || key.split('/').count() > 8 || !key.split('/').all(segment) {
        return Err(Error::new("Invalid live object key"));
    }
    Ok(())
}

// R2 conditions take only strong ETags: `If-Match: *` would overwrite any version.
fn etag(token: &str) -> Result<&str> {
    if strong_etag(token) {
        Ok(token)
    } else {
        Err(Error::new("Invalid live object ETag"))
    }
}

fn http_error(method: &str, status: u16) -> Error {
    Error::new(format!(
        "Live object {method} failed (HTTP {status}); check backend access and retry"
    ))
}

#[cfg(test)]
#[path = "objects_tests.rs"]
mod tests;
