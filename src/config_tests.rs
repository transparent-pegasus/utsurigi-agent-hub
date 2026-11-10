//! Tests for the per-hub machine hash.

use super::{Config, Machine};

fn config(hub: &str, machine: Option<&str>) -> Config {
    Config {
        hub: hub.into(),
        machine: Machine {
            id: machine.map(Into::into),
            root: None,
        },
        ..Config::default()
    }
}

#[test]
fn machine_hash_matches_vector() {
    // printf 'utsurigi-agent-hub machine v1\0default\0machine-1' | sha256sum
    assert_eq!(
        config("default", Some("machine-1"))
            .machine_hash()
            .as_deref(),
        Some("d2feecd78ab28004e8f35a66157d7d879c0ea3af7e4290b146877eb4cf1a4203")
    );
}

#[test]
fn machine_hash_depends_on_hub() {
    // printf 'utsurigi-agent-hub machine v1\0other\0machine-1' | sha256sum
    let other = config("other", Some("machine-1")).machine_hash();
    assert_eq!(
        other.as_deref(),
        Some("8fb006d5644dc7ca44c14f2477bec3d5e1a94dfb9d6eec5102cadeb8cc155954")
    );
    assert_ne!(other, config("default", Some("machine-1")).machine_hash());
}

#[test]
fn machine_hash_is_none_without_id() {
    assert_eq!(config("default", None).machine_hash(), None);
}
