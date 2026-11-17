#[allow(dead_code)]
mod common;

use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

const TRANSFER: &str = "Store complete quiescent encrypted repositories in private Google Drive folders; recover only verified generations.";
const MISSING_TRANSFER: &str = "Missing configuration: storage.bot_transfer";
const S3_NEEDS_R2: &str = "s3: repository URLs require storage.provider: r2 (an omitted provider means google-drive); google-drive and local use absolute local paths";

fn report(fixture: &Fixture, args: &[&str]) -> Value {
    serde_json::from_slice(&fixture.ok(&fixture.alpha, args).stdout).unwrap()
}

fn refused(output: Output) -> String {
    failure(&output);
    String::from_utf8(output.stderr)
        .unwrap()
        .trim_end()
        .to_owned()
}

// Every directory and file digest beneath root, so any write shows up as a difference.
fn tree(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            entries.extend(tree(&path));
            entries.insert(path, "directory".to_owned());
        } else {
            let digest = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            entries.insert(path, digest);
        }
    }
    entries
}

#[test]
fn google_drive_without_a_transfer_policy_is_not_ready_and_creates_no_repository() {
    let mut fixture = Fixture::new();
    for policy in [Value::Null, json!(""), json!(" \t ")] {
        fixture.alpha.config["storage"]["bot_transfer"] = policy.clone();
        fixture.alpha.save();
        assert_eq!(
            report(&fixture, &["validate", "--template"]),
            json!({"valid": true, "ready": false}),
            "google-drive with storage.bot_transfer {policy} must not be ready"
        );
        let status = report(&fixture, &["status"]);
        assert_eq!(status["ready"], false);
        assert_eq!(status["next_action"], "configure");
        assert_eq!(
            status["remediation"],
            json!(["Set storage.bot_transfer in configuration"])
        );
        for operation in ["validate", "setup", "sync"] {
            assert_eq!(
                refused(fixture.cli(&fixture.alpha, &[operation])),
                MISSING_TRANSFER,
                "{operation} with storage.bot_transfer {policy}"
            );
        }
        for name in ["shared", "bot"] {
            assert!(
                !Path::new(fixture.alpha.repository(name)).exists(),
                "{name} repository created without a transfer policy"
            );
        }
    }
    fixture.alpha.config["storage"] = json!({"provider": "local", "bot_transfer": null});
    fixture.alpha.save();
    assert_eq!(
        report(&fixture, &["validate"]),
        json!({"valid": true, "ready": true}),
        "local needs no transfer policy"
    );
    fixture.alpha.config["storage"] = json!({"provider": "google-drive", "bot_transfer": TRANSFER});
    fixture.alpha.save();
    assert_eq!(
        report(&fixture, &["validate"]),
        json!({"valid": true, "ready": true})
    );
    assert_eq!(report(&fixture, &["status"])["ready"], true);
    fixture.ok(&fixture.alpha, &["setup"]);
    for name in ["shared", "bot"] {
        assert!(
            Path::new(fixture.alpha.repository(name))
                .join("config")
                .is_file()
        );
    }
}

#[test]
fn s3_urls_on_local_backed_providers_name_the_r2_requirement() {
    let mut fixture = Fixture::new();
    fixture.alpha.config["repositories"]["shared"]["url"] = json!(R2_SHARED);
    for storage in [
        json!({"provider": "local"}),
        json!({"provider": "google-drive"}),
        json!({}),
    ] {
        fixture.alpha.config["storage"] = storage.clone();
        fixture.alpha.save();
        assert_eq!(
            refused(fixture.cli(&fixture.alpha, &["validate"])),
            S3_NEEDS_R2,
            "storage {storage}"
        );
    }
}

#[test]
fn registry_s3_entries_name_the_entry_and_keep_the_operation_category() {
    let fixture = Fixture::new();
    let mut beta = fixture.bot("beta", "beta-1");
    beta.config["repositories"]["bot"]["url"] =
        json!("s3:https://compat-acceptance-42.r2.cloudflarestorage.com/fake/beta");
    fixture.registry(&fixture.alpha, &[&fixture.alpha, &beta]);
    assert_eq!(
        refused(fixture.cli(&fixture.alpha, &["setup"])),
        format!("Registry entry beta: {S3_NEEDS_R2}")
    );
    let recorded = &report(&fixture, &["status"])["failure"];
    assert_eq!(recorded["operation"], "setup");
    // The entry prefix must not reclassify a configuration error as shared data.
    assert_eq!(recorded["category"], "operation");
}

#[test]
fn restore_into_a_repository_is_refused_and_writes_nothing() {
    let fixture = Fixture::new();
    fixture.ok(&fixture.alpha, &["setup"]);
    let snapshot = fixture.current_shared_id(&fixture.alpha);
    let repositories = fixture.temp.path().join("repositories");
    let before = tree(&repositories);
    for name in ["shared", "bot"] {
        let destination = Path::new(fixture.alpha.repository(name)).join("restored");
        let output = fixture.cli(
            &fixture.alpha,
            &[
                "restore",
                "--repository",
                "shared",
                "--snapshot",
                &snapshot,
                "--destination",
                destination.to_str().unwrap(),
            ],
        );
        assert_eq!(
            refused(output),
            "Restore destination must be separate from repositories",
            "destination under the {name} repository"
        );
    }
    assert_eq!(
        tree(&repositories),
        before,
        "a refused restore must write nothing"
    );
}
