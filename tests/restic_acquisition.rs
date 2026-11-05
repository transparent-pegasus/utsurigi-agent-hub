#[allow(dead_code)]
mod common;
#[path = "common/tls.rs"]
mod tls;

use bzip2::{Compression, write::BzEncoder};
use common::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

const WRONG_VERSION: &[u8] = br#"#!/bin/sh
for argument do
    case "$argument" in
        version|--version|-V)
            printf '%s\n' 'restic 0.18.0 compiled with synthetic fixture'
            exit 0
            ;;
    esac
done
exec "${0%/*}/real-restic" "$@"
"#;

fn compressed(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = BzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn publish_archive(curl: &Path, archive: &[u8]) {
    fs::write(curl.join("archive"), archive).unwrap();
    let name = fs::read_to_string(curl.join("asset-name")).unwrap();
    fs::write(
        curl.join("checksums"),
        format!("{:x}  {name}\n", Sha256::digest(archive)),
    )
    .unwrap();
}

fn acquisition_fixture() -> (Fixture, PathBuf, PathBuf) {
    let mut fixture = Fixture::new();
    let tool = Fixture::wrapper(&mut fixture.alpha);
    let curl = tool.join("download");
    fs::create_dir(&curl).unwrap();
    fs::write(
        curl.join("curl"),
        include_str!("fixtures/curl-acquisition.sh"),
    )
    .unwrap();
    fs::set_permissions(curl.join("curl"), fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["cat", "chmod", "dirname", "env"] {
        let executable = Path::new("/usr/bin").join(name);
        assert!(executable.is_file(), "test prerequisite: {executable:?}");
        symlink(executable, curl.join(name)).unwrap();
    }
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => panic!("unsupported restic acquisition test architecture: {arch}"),
    };
    fs::write(
        curl.join("asset-name"),
        format!("restic_0.19.1_{os}_{arch}.bz2"),
    )
    .unwrap();
    fs::write(curl.join("mode"), "pass").unwrap();
    fs::write(curl.join("partial"), b"interrupted transfer").unwrap();
    fs::write(curl.join("requests"), "").unwrap();
    publish_archive(&curl, &compressed(&fs::read(tool.join("restic")).unwrap()));
    fs::remove_file(tool.join("restic")).unwrap();
    (fixture, tool, curl)
}

fn invoke(fixture: &Fixture, curl: &Path, command: &str) -> Output {
    // Only the fake curl and the existing restic fixture's utilities are reachable.
    fixture.cli_env(
        &fixture.alpha,
        &[command],
        &[("PATH", curl.to_str().unwrap())],
    )
}

fn no_repositories(fixture: &Fixture) {
    for repository in ["shared", "bot"] {
        assert!(
            !Path::new(fixture.alpha.repository(repository))
                .join("config")
                .exists(),
            "{repository} was initialized despite rejected acquisition"
        );
    }
}

fn rejected_setup(fixture: &Fixture, tool: &Path, curl: &Path) {
    failure(&invoke(fixture, curl, "setup"));
    assert!(
        tool.join("restic").symlink_metadata().is_err(),
        "rejected acquisition left a promoted executable"
    );
    no_repositories(fixture);
    assert_no_writes(tool);
    let requests = fs::read_to_string(curl.join("requests")).unwrap();
    assert!(
        requests.lines().any(|url| {
            url == "https://github.com/restic/restic/releases/download/v0.19.1/SHA256SUMS"
        }),
        "setup never reached official checksum acquisition: {requests}"
    );
}

#[test]
fn setup_rejects_mismatched_or_missing_checksums_before_installing() {
    for matching_entry in [true, false] {
        let (fixture, tool, curl) = acquisition_fixture();
        let name = fs::read_to_string(curl.join("asset-name")).unwrap();
        let entry = if matching_entry {
            name.as_str()
        } else {
            "unrelated-release.bz2"
        };
        fs::write(
            curl.join("checksums"),
            format!("{}  {entry}\n", "0".repeat(64)),
        )
        .unwrap();
        rejected_setup(&fixture, &tool, &curl);
    }
}

#[test]
fn setup_rejects_failed_downloads_and_checksummed_incomplete_archives() {
    for mode in ["checksum-failure", "archive-failure", "incomplete-archive"] {
        let (fixture, tool, curl) = acquisition_fixture();
        if mode == "incomplete-archive" {
            let mut archive = fs::read(curl.join("archive")).unwrap();
            archive.truncate(archive.len() / 2);
            publish_archive(&curl, &archive);
        } else {
            fs::write(curl.join("mode"), mode).unwrap();
        }
        rejected_setup(&fixture, &tool, &curl);
    }
}

#[test]
fn setup_rejects_a_checksummed_download_with_the_wrong_executable_version() {
    let (fixture, tool, curl) = acquisition_fixture();
    publish_archive(&curl, &compressed(WRONG_VERSION));
    rejected_setup(&fixture, &tool, &curl);
}

#[test]
fn setup_and_sync_preserve_an_existing_incompatible_executable() {
    let (fixture, tool, curl) = acquisition_fixture();
    let executable = tool.join("restic");
    fs::write(&executable, WRONG_VERSION).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    for command in ["setup", "sync"] {
        failure(&invoke(&fixture, &curl, command));
        assert_eq!(fs::read(&executable).unwrap(), WRONG_VERSION);
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(fs::read(curl.join("requests")).unwrap().is_empty());
        no_repositories(&fixture);
    }
}

#[test]
fn ordinary_sync_never_downloads_a_missing_executable() {
    let (fixture, tool, curl) = acquisition_fixture();
    failure(&invoke(&fixture, &curl, "sync"));
    assert!(fs::read(curl.join("requests")).unwrap().is_empty());
    assert!(!tool.join("restic").exists());
    no_repositories(&fixture);
    assert_no_writes(&tool);
}

#[test]
fn setup_installs_a_verified_download_and_sync_reuses_it() {
    let (fixture, tool, curl) = acquisition_fixture();
    success(&invoke(&fixture, &curl, "setup"));
    assert_eq!(
        fs::read(tool.join("restic")).unwrap(),
        include_bytes!("fixtures/restic-wrapper.sh")
    );
    assert_ne!(
        fs::metadata(tool.join("restic"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    for repository in ["shared", "bot"] {
        assert!(
            Path::new(fixture.alpha.repository(repository))
                .join("config")
                .is_file()
        );
    }
    let requests = fs::read_to_string(curl.join("requests")).unwrap();
    let name = fs::read_to_string(curl.join("asset-name")).unwrap();
    for asset in ["SHA256SUMS", name.as_str()] {
        assert!(requests.lines().any(|url| {
            url == format!("https://github.com/restic/restic/releases/download/v0.19.1/{asset}")
        }));
    }
    success(&invoke(&fixture, &curl, "sync"));
    assert_eq!(fs::read_to_string(curl.join("requests")).unwrap(), requests);
}

#[test]
fn acquisition_children_receive_dotenv_connections_without_repository_or_aws_credentials() {
    let (fixture, tool, curl) = acquisition_fixture();
    // Keep TLS intact through real restic's nested migration helper.
    let ca = tls::write_pem(fixture.temp.path(), "acquisition-ca.pem");
    let client = tls::write_pem(fixture.temp.path(), "acquisition-client.pem");
    let marker = fixture.temp.path().join("must-not-execute");
    let poison_repository = fixture.temp.path().join("wrong-repository");
    let mut document = CONNECTION_ENV
        .iter()
        .map(|(name, value)| format!("{name}={value}\n"))
        .collect::<String>();
    document.push_str(&format!(
        "AWS_ACCESS_KEY_ID=synthetic-access-key\nAWS_SECRET_ACCESS_KEY={BOT_PASSWORD}\nAWS_SESSION_TOKEN={PEER_PASSWORD}\nRESTIC_PASSWORD=synthetic-ambient-password\nRESTIC_REPOSITORY={}\nRESTIC_PASSWORD_COMMAND=touch {}\nRESTIC_REPOSITORY_FILE=/synthetic/unwanted-repository-file\nRESTIC_UNKNOWN_CONTROL=unwanted\nTEST_HUB_UNRELATED_SECRET=synthetic-unrelated-secret\n",
        poison_repository.display(), marker.display(),
    ));
    document.push_str(&format!(
        "RESTIC_CACERT={}\nRESTIC_TLS_CLIENT_CERT={}\n",
        ca.display(),
        client.display(),
    ));
    dotenv(&fixture.alpha, document);
    let output = invoke(&fixture, &curl, "setup");
    success(&output);
    let boundary = fs::read_to_string(curl.join("boundary")).unwrap();
    let calls: Vec<_> = boundary.split("__CALL__\n").skip(1).collect();
    assert_eq!(
        calls.len(),
        2,
        "both checksum and archive downloads must be checked"
    );
    for call in calls {
        for (name, value) in CONNECTION_ENV {
            assert_eq!(
                child_env(call, name),
                Some(value),
                "connection variable {name}"
            );
        }
        assert_eq!(child_env(call, "PATH"), curl.to_str());
        for (name, value) in [("LANG", "C.UTF-8"), ("LC_ALL", "C.UTF-8"), ("TZ", "UTC")] {
            assert_eq!(child_env(call, name), Some(value));
        }
        for name in [
            "TEST_HUB_SHARED_PASSWORD",
            "TEST_HUB_ALPHA_PASSWORD",
            "TEST_HUB_BETA_PASSWORD",
            "TEST_HUB_UNRELATED_SECRET",
            "RESTIC_PASSWORD",
            "RESTIC_PASSWORD_COMMAND",
            "RESTIC_REPOSITORY",
            "RESTIC_REPOSITORY_FILE",
            "RESTIC_UNKNOWN_CONTROL",
            "RESTIC_CACERT",
            "RESTIC_TLS_CLIENT_CERT",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
        ] {
            assert!(
                child_env(call, name).is_none(),
                "acquisition inherited {name}"
            );
        }
    }
    let restic_boundary = fs::read_to_string(tool.join("boundary")).unwrap();
    let mut versions = 0;
    for call in restic_boundary.split("__CALL__\n").skip(1) {
        if call.lines().any(|line| line == "arg:version") {
            versions += 1;
            for name in ["RESTIC_CACERT", "RESTIC_TLS_CLIENT_CERT"] {
                assert!(
                    child_env(call, name).is_none(),
                    "version check inherited {name}"
                );
            }
        } else {
            for (name, value) in [
                ("RESTIC_CACERT", ca.to_str().unwrap()),
                ("RESTIC_TLS_CLIENT_CERT", client.to_str().unwrap()),
            ] {
                assert_eq!(
                    child_env(call, name),
                    Some(value),
                    "storage connection {name}"
                );
            }
        }
    }
    assert!(
        versions > 0,
        "downloaded restic compatibility must be checked"
    );
    assert!(!marker.exists());
    assert!(!poison_repository.exists());
    assert!(tool.join("restic").is_file());
    assert_eq!(
        fs::metadata(curl.join("boundary"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
