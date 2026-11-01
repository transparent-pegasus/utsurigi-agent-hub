use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
mod restic;
mod temp;
pub use temp::TempDir;

pub const SHARED_PASSWORD: &str = "synthetic-SHARED-secret-7f0b";
pub const BOT_PASSWORD: &str = "synthetic-ALPHA-secret-9c2d";
pub const PEER_PASSWORD: &str = "synthetic-BETA-secret-3e4a";
pub const PAYLOAD: &[u8] = "日本語の記憶 🐎\nunique-private-payload-9f3b\0tail".as_bytes();

pub struct Bot {
    pub file: PathBuf,
    pub config: Value,
}

impl Bot {
    pub fn save(&self) {
        fs::write(&self.file, serde_json::to_vec_pretty(&self.config).unwrap()).unwrap();
    }

    pub fn path(&self, name: &str) -> PathBuf {
        let path = PathBuf::from(self.config["paths"][name].as_str().unwrap());
        if path.is_absolute() {
            path
        } else {
            self.file.parent().unwrap().join(path)
        }
    }

    pub fn repository(&self, name: &str) -> &str {
        self.config["repositories"][name]["url"].as_str().unwrap()
    }

    pub fn entry(&self) -> Value {
        let repository = &self.config["repositories"]["bot"];
        json!({
            "id": self.config["bot"]["id"],
            "runtime_id": self.config["bot"]["runtime_id"],
            "repository": repository["url"],
            "password_ref": repository["password_ref"],
            "password_env": repository["password_env"],
            "shared": self.config["sharing"]["bot_data"],
        })
    }
}

pub struct Fixture {
    pub temp: TempDir,
    pub alpha: Bot,
    _restic_guard: MutexGuard<'static, ()>,
}

impl Fixture {
    pub fn new() -> Self {
        static RESTIC: Mutex<()> = Mutex::new(());
        let guard = RESTIC.lock().unwrap_or_else(|error| error.into_inner());
        let temp = TempDir::new();
        let alpha = Self::make_bot(temp.path(), "alpha", "alpha-1");
        let fixture = Self {
            temp,
            alpha,
            _restic_guard: guard,
        };
        fixture.registry(&fixture.alpha, &[&fixture.alpha]);
        fixture
    }

    pub fn make_bot(root: &Path, id: &str, runtime: &str) -> Bot {
        let local = root.join(runtime);
        let owned = local.join("owned");
        let shared = local.join("shared");
        fs::create_dir_all(owned.join("notes")).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(owned.join("notes/日本語.md"), PAYLOAD).unwrap();
        let password_env = if id == "alpha" {
            "TEST_HUB_ALPHA_PASSWORD"
        } else {
            "TEST_HUB_BETA_PASSWORD"
        };
        let bot = Bot {
            file: local.join("config.yaml"),
            config: json!({
                "version": 1,
                "hub": "default",
                "timezone": "UTC",
                "bot": {"id": id, "platform": "test", "runtime_id": runtime},
                "storage": {"provider": "local"},
                "encryption": {"executable": restic::executable(), "version": "0.19.1"},
                "repositories": {
                    "shared": {
                        "url": root.join("repositories/shared"),
                        "password_env": "TEST_HUB_SHARED_PASSWORD",
                        "password_ref": "op://synthetic/shared/password"
                    },
                    "bot": {
                        "url": root.join("repositories").join(id),
                        "password_env": password_env,
                        "password_ref": format!("op://synthetic/{id}/password")
                    }
                },
                "paths": {
                    "owned": owned, "shared": shared,
                    "imports": local.join("imports"), "state": local.join("state")
                },
                "data": {"include": ["notes"]},
                "sharing": {"bot_data": true}
            }),
        };
        bot.save();
        bot
    }

    pub fn bot(&self, id: &str, runtime: &str) -> Bot {
        Self::make_bot(self.temp.path(), id, runtime)
    }

    pub fn registry(&self, owner: &Bot, bots: &[&Bot]) {
        let registry = json!({
            "version": 1,
            "hub": owner.config["hub"].as_str().unwrap_or("default"),
            "owner": {
                "bot_id": owner.config["bot"]["id"],
                "runtime_id": owner.config["bot"]["runtime_id"]
            },
            "bots": bots.iter().map(|bot| bot.entry()).collect::<Vec<_>>()
        });
        fs::write(
            owner.path("shared").join("registry.yaml"),
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();
    }

    fn command(&self, executable: impl AsRef<Path>) -> Command {
        let mut command = Command::new(executable.as_ref());
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("TMPDIR", self.temp.path())
            .env("XDG_CACHE_HOME", self.temp.path().join("cache"))
            .env("RESTIC_CACHE_DIR", self.temp.path().join("restic-cache"))
            .env("TEST_HUB_SHARED_PASSWORD", SHARED_PASSWORD)
            .env("TEST_HUB_ALPHA_PASSWORD", BOT_PASSWORD)
            .env("TEST_HUB_BETA_PASSWORD", PEER_PASSWORD)
            .current_dir(self.temp.path());
        command
    }

    pub fn cli(&self, bot: &Bot, args: &[&str]) -> Output {
        self.cli_env(bot, args, &[])
    }

    pub fn cli_command(&self, bot: &Bot, args: &[&str]) -> Command {
        let mut command = self.command(env!("CARGO_BIN_EXE_utsurigi-agent-hub"));
        command.arg("--config").arg(&bot.file).args(args);
        command
    }

    pub fn cli_env(&self, bot: &Bot, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.cli_command(bot, args);
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().expect("utsurigi-agent-hub must execute")
    }

    pub fn ok(&self, bot: &Bot, args: &[&str]) -> Output {
        let output = self.cli(bot, args);
        success(&output);
        output
    }

    pub fn status(&self, bot: &Bot) -> Value {
        let output = self.ok(bot, &["status"]);
        let status: Value = serde_json::from_slice(&output.stdout).expect("status must emit JSON");
        assert_eq!(
            status["hub"],
            bot.config["hub"].as_str().unwrap_or("default")
        );
        assert!(status["ready"].is_boolean());
        for field in ["last_success", "anchor", "next_due"] {
            assert!(status.get(field).is_some(), "missing status field {field}");
            assert!(
                status[field].is_null()
                    || status[field]
                        .as_str()
                        .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok()),
                "{field} must be RFC3339 or null: {}",
                status[field]
            );
        }
        assert!(
            status.get("active_shared").is_some(),
            "missing active_shared"
        );
        assert!(status["active_shared"].is_null() || status["active_shared"].is_string());
        json!({
            "hub": status["hub"],
            "ready": status["ready"], "last_success": status["last_success"],
            "anchor": status["anchor"], "next_due": status["next_due"],
            "active_shared": status["active_shared"]
        })
    }

    pub fn active_shared(&self, bot: &Bot) -> PathBuf {
        let status = self.status(bot);
        let path = PathBuf::from(
            status["active_shared"]
                .as_str()
                .expect("complete shared import"),
        );
        let path = path.canonicalize().unwrap();
        assert!(path.is_dir());
        assert!(path.starts_with(bot.path("imports").canonicalize().unwrap()));
        assert!(!path.starts_with(bot.path("owned").canonicalize().unwrap()));
        assert!(!path.starts_with(bot.path("shared").canonicalize().unwrap()));
        assert!(path.join("registry.yaml").is_file());
        path
    }

    pub fn restic(&self, bot: &Bot, repository: &str, args: &[&str]) -> Output {
        let password = if repository == "shared" {
            SHARED_PASSWORD
        } else if bot.config["bot"]["id"] == "alpha" {
            BOT_PASSWORD
        } else {
            PEER_PASSWORD
        };
        self.command(restic::executable())
            .current_dir(bot.path("shared"))
            .env("RESTIC_PASSWORD", password)
            .arg("--repo")
            .arg(bot.repository(repository))
            .args(args)
            .output()
            .expect("real restic 0.19.1 must be available; this test cannot be skipped")
    }

    pub fn snapshots(&self, bot: &Bot, repository: &str) -> Vec<Value> {
        let output = self.restic(bot, repository, &["snapshots", "--json"]);
        success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn ids(&self, bot: &Bot, repository: &str) -> BTreeSet<String> {
        self.snapshots(bot, repository)
            .iter()
            .map(|snapshot| snapshot["id"].as_str().unwrap().to_owned())
            .collect()
    }

    pub fn wrapper(bot: &mut Bot) -> PathBuf {
        let directory = bot.file.parent().unwrap().join("restic-fixture");
        fs::create_dir_all(&directory).unwrap();
        symlink(restic::executable(), directory.join("real-restic")).unwrap();
        let executable = directory.join("restic");
        fs::write(&executable, include_str!("../fixtures/restic-wrapper.sh")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.join("mode"), "pass").unwrap();
        fs::write(directory.join("calls"), "").unwrap();
        bot.config["encryption"]["executable"] = json!(executable);
        bot.save();
        directory
    }
}

pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub fn success(output: &Output) {
    assert!(output.status.success(), "{}", text(output));
    redacted(output);
}

pub fn failure(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        text(output)
    );
    redacted(output);
}

pub fn redacted(output: &Output) {
    let output = text(output);
    for secret in [SHARED_PASSWORD, BOT_PASSWORD, PEER_PASSWORD] {
        assert!(
            !output.contains(secret),
            "output exposed a synthetic secret"
        );
    }
}

pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

pub fn assert_no_plaintext(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_plaintext(&path);
        } else {
            let bytes = fs::read(path).unwrap();
            for needle in [PAYLOAD, BOT_PASSWORD.as_bytes(), SHARED_PASSWORD.as_bytes()] {
                assert!(
                    !bytes.windows(needle.len()).any(|window| window == needle),
                    "repository contains plaintext payload or password"
                );
            }
        }
    }
}

pub fn assert_no_writes(directory: &Path) {
    let calls = fs::read_to_string(directory.join("calls")).unwrap();
    for operation in ["backup", "init", "forget", "prune", "unlock"] {
        assert!(
            !calls.lines().any(|call| call == operation),
            "unexpected {operation}: {calls}"
        );
    }
}
