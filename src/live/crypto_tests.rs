//! Tests for live crypto, written from the Task 2 brief without reading the
//! implementation. The fixed vectors were sealed outside this crate, by the
//! reference libargon2 and by libsodium.

use super::{Key, Kind, SALT_BYTES, derive, open, salt, seal};
use crate::error::Result;
use std::sync::OnceLock;

const PASSWORD: &[u8] = b"synthetic-live-password";
const SALT: [u8; SALT_BYTES] = *b"live-test-salt16";
const HUB: &str = "default";
const BOT: &str = "alpha";
const BINDING: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const OTHER_BINDING: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const UNREADABLE: &str =
    "Live data cannot be decrypted; check the repository password and live list";
const NONCE: usize = 24;
const TAG: usize = 16;
const KINDS: [Kind; 3] = [Kind::Spec, Kind::Name, Kind::Object];

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("live crypto failed: {}", error.message))
}

fn error<T>(result: Result<T>) -> Option<String> {
    result.err().map(|error| error.message)
}

/// Argon2id at the fixed cost is slow without optimizations, so tests share one key.
fn key() -> &'static Key {
    static KEY: OnceLock<Key> = OnceLock::new();
    KEY.get_or_init(|| must(derive(PASSWORD, &SALT)))
}

fn decoded(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("vector hex"))
        .collect()
}

#[test]
fn salt_is_sixteen_fresh_bytes() {
    let first: [u8; 16] = must(salt());
    assert_ne!(first, must(salt()));
}

#[test]
fn derive_is_deterministic_and_input_sensitive() {
    let sealed = must(seal(key(), HUB, BOT, Kind::Object, None, b"body"));
    let again = must(derive(PASSWORD, &SALT));
    assert_eq!(
        must(open(&again, HUB, BOT, Kind::Object, None, &sealed)),
        b"body"
    );
    let resealed = must(seal(&again, HUB, BOT, Kind::Object, None, b"body"));
    assert_eq!(
        must(open(key(), HUB, BOT, Kind::Object, None, &resealed)),
        b"body"
    );
    let other_salt = must(derive(PASSWORD, b"live-test-salt17"));
    let other_password = must(derive(b"synthetic-live-passwore", &SALT));
    for (case, other) in [("salt", &other_salt), ("password", &other_password)] {
        assert_eq!(
            error(open(other, HUB, BOT, Kind::Object, None, &sealed)).as_deref(),
            Some(UNREADABLE),
            "different {case}"
        );
    }
}

#[test]
fn derive_rejects_empty_password() {
    assert_eq!(
        error(derive(b"", &SALT)).as_deref(),
        Some("Live key requires a repository password")
    );
}

/// Sealed by libsodium's XChaCha20-Poly1305 with the key libargon2 derives from
/// PASSWORD and SALT (Argon2id v0x13, m=19456 KiB, t=2, p=1, 32 bytes), the AAD
/// `"uah-live v1\0" hub "\0" bot "\0" kind ["\0" binding]`, and the layout
/// nonce ‖ ciphertext ‖ tag. A different KDF, cipher, AAD, or layout fails here.
#[test]
fn open_accepts_independently_sealed_vectors() {
    let vectors: [(Kind, Option<&str>, &str, &[u8]); 3] = [
        (
            Kind::Spec,
            None,
            "000102030405060708090a0b0c0d0e0f1011121314151617\
             70f89aac2e5e5dda9a344fc90e79141e5af6f06dfa6535b3386bad94551b311b\
             3c1006084c6e39907f9ab2319255f50d20a103b08f584ade54a25452",
            br#"{"roots":[{"path":"tasks/","class":"task"}]}"#,
        ),
        (
            Kind::Name,
            Some(BINDING),
            "18191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f\
             3de9e82de27f27187d598355525ae4a4e7b237f750637531f3bf",
            b"tasks/a.md",
        ),
        (
            Kind::Object,
            None,
            "303132333435363738393a3b3c3d3e3f4041424344454647\
             486ebb62fcea079d71449f3b94f50be6117d0feb1b605c714c885463",
            b"object body\n",
        ),
    ];
    for (kind, binding, sealed, plaintext) in vectors {
        let opened = open(key(), HUB, BOT, kind, binding, &decoded(sealed));
        assert_eq!(opened.ok(), Some(plaintext.to_vec()), "{kind:?} vector");
    }
}

#[test]
fn seal_round_trips_with_binding_and_without() {
    let plaintexts: [&[u8]; 3] = [b"", b"tasks/a.md", &[0, 0xff, b'\n', b'\r']];
    for kind in KINDS {
        for binding in [None, Some(BINDING)] {
            for plaintext in plaintexts {
                let sealed = must(seal(key(), HUB, BOT, kind, binding, plaintext));
                assert_eq!(
                    must(open(key(), HUB, BOT, kind, binding, &sealed)),
                    plaintext,
                    "{kind:?} {binding:?}"
                );
            }
        }
    }
}

#[test]
fn seal_uses_fresh_nonces() {
    let plaintext = b"tasks/a.md";
    let first = must(seal(key(), HUB, BOT, Kind::Name, Some(BINDING), plaintext));
    let second = must(seal(key(), HUB, BOT, Kind::Name, Some(BINDING), plaintext));
    assert_ne!(first, second);
    // Random 24-byte nonces differ almost everywhere; a counter or a clock would not.
    let differing = first[..NONCE]
        .iter()
        .zip(&second[..NONCE])
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        differing > NONCE / 2,
        "nonces differ in only {differing} of {NONCE} bytes"
    );
    for sealed in [first, second] {
        assert_eq!(
            must(open(key(), HUB, BOT, Kind::Name, Some(BINDING), &sealed)),
            plaintext
        );
    }
}

#[test]
fn sealed_length_is_nonce_plus_tag() {
    for length in [0, 1, 64, 1000] {
        let sealed = must(seal(
            key(),
            HUB,
            BOT,
            Kind::Object,
            None,
            &vec![0x5a; length],
        ));
        assert_eq!(
            sealed.len(),
            NONCE + length + TAG,
            "{length}-byte plaintext"
        );
    }
}

#[test]
fn open_rejects_tampered_byte() {
    let plaintext = b"object body\n";
    let sealed = must(seal(key(), HUB, BOT, Kind::Object, None, plaintext));
    assert_eq!(
        must(open(key(), HUB, BOT, Kind::Object, None, &sealed)),
        plaintext
    );
    let last = sealed.len() - 1;
    for (place, at) in [
        ("first nonce byte", 0),
        ("a ciphertext byte", NONCE + 3),
        ("last tag byte", last),
    ] {
        let mut tampered = sealed.clone();
        tampered[at] ^= 0x01;
        assert_eq!(
            error(open(key(), HUB, BOT, Kind::Object, None, &tampered)).as_deref(),
            Some(UNREADABLE),
            "{place}"
        );
    }
    let extended = [sealed.as_slice(), &[0]].concat();
    for (place, tampered) in [
        ("truncated", &sealed[..last]),
        ("extended", extended.as_slice()),
    ] {
        assert_eq!(
            error(open(key(), HUB, BOT, Kind::Object, None, tampered)).as_deref(),
            Some(UNREADABLE),
            "{place}"
        );
    }
}

#[test]
fn open_rejects_mismatched_context() {
    let plaintext = b"tasks/a.md";
    let sealed = must(seal(key(), HUB, BOT, Kind::Name, Some(BINDING), plaintext));
    assert_eq!(
        must(open(key(), HUB, BOT, Kind::Name, Some(BINDING), &sealed)),
        plaintext
    );
    for (case, hub, bot, binding) in [
        ("hub", "other", BOT, Some(BINDING)),
        ("bot", HUB, "beta", Some(BINDING)),
        ("binding Some opened as None", HUB, BOT, None),
        ("binding value", HUB, BOT, Some(OTHER_BINDING)),
    ] {
        assert_eq!(
            error(open(key(), hub, bot, Kind::Name, binding, &sealed)).as_deref(),
            Some(UNREADABLE),
            "{case}"
        );
    }
    let unbound = must(seal(key(), HUB, BOT, Kind::Name, None, plaintext));
    assert_eq!(
        error(open(key(), HUB, BOT, Kind::Name, Some(BINDING), &unbound)).as_deref(),
        Some(UNREADABLE),
        "binding None opened as Some"
    );
    for (sealed_at, sealed_kind) in KINDS.into_iter().enumerate() {
        let sealed = must(seal(key(), HUB, BOT, sealed_kind, None, plaintext));
        for (opened_at, opened_kind) in KINDS.into_iter().enumerate() {
            if sealed_at != opened_at {
                assert_eq!(
                    error(open(key(), HUB, BOT, opened_kind, None, &sealed)).as_deref(),
                    Some(UNREADABLE),
                    "{sealed_kind:?} opened as {opened_kind:?}"
                );
            }
        }
    }
}

#[test]
fn open_rejects_short_input() {
    let sealed = must(seal(key(), HUB, BOT, Kind::Object, None, b""));
    assert_eq!(sealed.len(), NONCE + TAG);
    assert_eq!(
        must(open(key(), HUB, BOT, Kind::Object, None, &sealed)),
        b""
    );
    for length in [NONCE + TAG - 1, NONCE, 1, 0] {
        assert_eq!(
            error(open(key(), HUB, BOT, Kind::Object, None, &sealed[..length])).as_deref(),
            Some(UNREADABLE),
            "{length} bytes"
        );
    }
}

/// Checked at compile time: the call is ambiguous, so this file stops compiling,
/// if `Key` ever gains `Debug` or `Display`.
#[test]
fn key_has_no_debug_or_display() {
    trait AmbiguousIfPrintable<Marker> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfPrintable<()> for T {}
    impl<T: ?Sized + std::fmt::Debug> AmbiguousIfPrintable<u8> for T {}
    impl<T: ?Sized + std::fmt::Display> AmbiguousIfPrintable<u16> for T {}
    <Key as AmbiguousIfPrintable<_>>::check();
}
