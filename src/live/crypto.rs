//! Live key derivation (Argon2id) and authenticated encryption (XChaCha20-Poly1305).

use crate::error::{Error, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, Generate, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

pub const SALT_BYTES: usize = 16;
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;

pub struct Key([u8; 32]);

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Spec,
    Name,
    Object,
}

pub fn salt() -> Result<[u8; SALT_BYTES]> {
    <[u8; SALT_BYTES]>::try_generate().map_err(|_| no_randomness())
}

pub fn derive(password: &[u8], salt: &[u8; SALT_BYTES]) -> Result<Key> {
    if password.is_empty() {
        return Err(Error::new("Live key requires a repository password"));
    }
    let mut key = [0; 32];
    Params::new(19_456, 2, 1, Some(key.len()))
        .and_then(|params| {
            Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
                .hash_password_into(password, salt, &mut key)
        })
        .map_err(|_| Error::new("Cannot derive the live key"))?;
    Ok(Key(key))
}

pub fn seal(
    key: &Key,
    hub: &str,
    bot: &str,
    kind: Kind,
    binding: Option<&str>,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let nonce = XNonce::try_generate().map_err(|_| no_randomness())?;
    let ciphertext = XChaCha20Poly1305::new(&key.0.into())
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad(hub, bot, kind, binding),
            },
        )
        .map_err(|_| Error::new("Cannot encrypt live data"))?;
    Ok([nonce.as_slice(), &ciphertext].concat())
}

pub fn open(
    key: &Key,
    hub: &str,
    bot: &str,
    kind: Kind,
    binding: Option<&str>,
    sealed: &[u8],
) -> Result<Vec<u8>> {
    let undecryptable =
        || Error::new("Live data cannot be decrypted; check the repository password and live list");
    let (nonce, ciphertext) = sealed
        .split_first_chunk::<NONCE_BYTES>()
        .filter(|(_, rest)| rest.len() >= TAG_BYTES)
        .ok_or_else(undecryptable)?;
    XChaCha20Poly1305::new(&key.0.into())
        .decrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: ciphertext,
                aad: &aad(hub, bot, kind, binding),
            },
        )
        .map_err(|_| undecryptable())
}

fn aad(hub: &str, bot: &str, kind: Kind, binding: Option<&str>) -> Vec<u8> {
    let kind = match kind {
        Kind::Spec => "spec",
        Kind::Name => "name",
        Kind::Object => "object",
    };
    let mut parts = vec!["uah-live v1", hub, bot, kind];
    parts.extend(binding);
    parts.join("\0").into_bytes()
}

fn no_randomness() -> Error {
    Error::new("Operating system randomness is unavailable")
}

#[cfg(test)]
#[path = "crypto_tests.rs"]
mod tests;
