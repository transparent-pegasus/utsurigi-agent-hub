//! Tests for registry machines and runtime roles.

use super::{Entry, Registry, Role};
use crate::config::Config;
use crate::error::Result;
use crate::selection::Export;
use std::fs;

const REPOSITORY: &str = "s3:https://r2.test/registry";
// printf 'utsurigi-agent-hub machine v1\0default\0machine-N' | sha256sum
const MACHINE_1: &str = "d2feecd78ab28004e8f35a66157d7d879c0ea3af7e4290b146877eb4cf1a4203";
const MACHINE_2: &str = "160acb3ae261c20961a0ae7243bf4b84b6eb9968a1ba72dce388d0680615abad";
const MACHINE_3: &str = "3e7e617498e0ca19726a943358b496e33f05916e59b2797c4ba18d7f2a50b91d";

// `Registry::write` output for `registry(&[], &[])`, captured at 6d1b080 before
// `machines` existed.
const PRE_CHANGE: &str = "\
version: 2
hub: default
max_bots: 50
bots:
- id: alpha
  runtime_id: alpha-runtime
  repository: s3:https://r2.test/registry/alpha
  password_ref: op://fixture/alpha/password
  password_env: UAH_ALPHA_PASSWORD
  shared: true
- id: beta
  runtime_id: beta-runtime
  repository: s3:https://r2.test/registry/beta
  password_ref: null
  password_env: UAH_BETA_PASSWORD
  shared: false
";

fn must<T>(result: Result<T>) -> T {
    result.unwrap_or_else(|error| panic!("registry operation failed: {}", error.message))
}

fn config(bot: &str, runtime: &str, machine: Option<&str>) -> Config {
    let mut config = Config {
        hub: "default".into(),
        ..Config::default()
    };
    config.bot.id = Some(bot.into());
    config.bot.runtime_id = Some(runtime.into());
    config.machine.id = machine.map(Into::into);
    config.repositories.bot.url = Some(format!("{REPOSITORY}/{bot}"));
    config.repositories.bot.password_ref = Some(format!("op://fixture/{bot}/password"));
    config.repositories.bot.password_env = Some(format!("UAH_{}_PASSWORD", bot.to_uppercase()));
    config
}

fn registry(alpha: &[&str], beta: &[&str]) -> Registry {
    Registry {
        version: 2,
        hub: "default".into(),
        max_bots: 50,
        bots: vec![
            Entry {
                id: "alpha".into(),
                runtime_id: "alpha-runtime".into(),
                repository: format!("{REPOSITORY}/alpha"),
                password_ref: Some("op://fixture/alpha/password".into()),
                password_env: "UAH_ALPHA_PASSWORD".into(),
                shared: true,
                machines: alpha.iter().map(|hash| hash.to_string()).collect(),
            },
            Entry {
                id: "beta".into(),
                runtime_id: "beta-runtime".into(),
                repository: format!("{REPOSITORY}/beta"),
                password_ref: None,
                password_env: "UAH_BETA_PASSWORD".into(),
                shared: false,
                machines: beta.iter().map(|hash| hash.to_string()).collect(),
            },
        ],
    }
}

fn write(registry: &Registry) -> (Export, String) {
    let mut export = Export {
        tree: tempfile::tempdir().unwrap(),
        bytes: 0,
    };
    must(registry.write(&mut export));
    let yaml = fs::read_to_string(export.tree.path().join("registry.yaml")).unwrap();
    (export, yaml)
}

#[test]
fn registry_without_machines_serializes_unchanged() {
    let rows = registry(&[], &[]);
    assert_eq!(write(&rows).1, PRE_CHANGE);

    // Registries written before `machines` existed still load, listing none.
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("registry.yaml"), PRE_CHANGE).unwrap();
    let loaded = must(Registry::load(
        root.path(),
        &config("alpha", "alpha-runtime", None),
    ));
    assert!(loaded == rows);
}

#[test]
fn registry_machines_round_trip() {
    let listed = registry(&[], &[MACHINE_2, MACHINE_3]);
    let (export, yaml) = write(&listed);
    // `machines` is the last Entry field, so beta's last machine ends the document.
    assert!(yaml.trim_end().ends_with(MACHINE_3), "{yaml}");
    let loaded = must(Registry::load(
        export.tree.path(),
        &config("alpha", "alpha-runtime", None),
    ));
    assert_eq!(loaded.bots[1].machines, [MACHINE_2, MACHINE_3]);
    assert!(loaded == listed);
}

#[test]
fn registry_rejects_bad_machines() {
    let config = config("alpha", "alpha-runtime", None);
    let load = |registry: &Registry| {
        let (export, _) = write(registry);
        Registry::load(export.tree.path(), &config).map(drop)
    };
    // Control: the same rows load with valid lists, so each rejection is the list's.
    must(load(&registry(&[MACHINE_1], &[MACHINE_2])));
    let upper = MACHINE_1.to_uppercase();
    let long = format!("{MACHINE_1}0");
    let not_hex = format!("{}g", &MACHINE_1[..63]);
    for bad in [
        vec![upper.as_str()],
        vec![&MACHINE_1[..63]],
        vec![MACHINE_1, MACHINE_1],
        vec![long.as_str()],
        vec![not_hex.as_str()],
        vec![""],
    ] {
        for (alpha, beta) in [(&bad[..], &[][..]), (&[][..], &bad[..])] {
            match load(&registry(alpha, beta)) {
                Err(error) => {
                    assert_eq!(error.message, "Invalid registry machine list", "{bad:?}")
                }
                Ok(()) => panic!("accepted machines {bad:?}"),
            }
        }
    }
}

#[test]
fn new_entry_lists_no_machines() {
    let root = tempfile::tempdir().unwrap();
    let config = config("alpha", "alpha-runtime", Some("machine-1"));
    let registry = must(Registry::initial(root.path(), &config, false));
    assert!(registry.bots[0].machines.is_empty());
}

#[test]
fn current_ignores_machines() {
    must(registry(&[MACHINE_2], &[]).current(&config("alpha", "alpha-runtime", Some("machine-1"))));
}

#[test]
fn role_primary_secondary_and_none() {
    fn derives<T: Clone + Copy + std::fmt::Debug + PartialEq + Eq>() {}
    derives::<Role>();
    let listed = registry(&[MACHINE_1, MACHINE_2], &[MACHINE_3]);
    let role = |bot: &str, runtime: &str, machine: Option<&str>| {
        listed.role(&config(bot, runtime, machine))
    };
    // The registered runtime is primary whatever the machine list says.
    assert_eq!(role("alpha", "alpha-runtime", None), Some(Role::Primary));
    assert_eq!(
        role("alpha", "alpha-runtime", Some("machine-1")),
        Some(Role::Primary)
    );
    assert_eq!(
        role("alpha", "other-runtime", Some("machine-2")),
        Some(Role::Secondary)
    );
    // machine-3 is listed for beta only.
    assert_eq!(role("alpha", "other-runtime", Some("machine-3")), None);
    assert_eq!(role("alpha", "other-runtime", None), None);
    assert_eq!(role("gamma", "gamma-runtime", Some("machine-2")), None);
}
