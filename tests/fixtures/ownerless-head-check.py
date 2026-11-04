#!/usr/bin/python3
"""Type-check independent head tests while the product module is absent.

This compiles metadata ONLY against PLAN Task 2 declarations and the real base
Config/error modules. The declaration methods panic and are NEVER executed.
This check is not backend RED/GREEN evidence. Product verification, after
inclusion by the head author, is:
  cargo test --locked --bin utsurigi-agent-hub head::tests:: -- --test-threads=1
"""

import ast
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    root = Path(__file__).resolve().parents[2]
    for path in (root / "tests/fixtures").glob("ownerless-head-*.py"):
        ast.parse(path.read_text(), filename=str(path))
        print("Python syntax OK:", path.name, flush=True)
    subprocess.run(
        ["cargo", "test", "--locked", "--offline", "--bin", "utsurigi-agent-hub", "--no-run"],
        cwd=root, timeout=120, check=True,
    )
    deps = root / "target/debug/deps"
    externs = []
    for name in ("serde", "serde_json", "serde_saphyr", "globset", "tempfile", "libc", "utsurigi_agent_hub"):
        candidates = list(deps.glob(f"lib{name}-*.rlib"))
        if not candidates:
            raise RuntimeError("missing cached build dependency " + name)
        artifact = max(candidates, key=lambda path: path.stat().st_mtime_ns)
        externs.extend(["--extern", f"{name}={artifact}"])
    # json.dumps gives Rust string literals for these ordinary absolute paths.
    source = """
#![allow(dead_code)]
#[path = ERROR] mod error;
mod selection {
    pub fn open_source(_: &std::path::Path, _: &std::path::Path)
        -> crate::error::Result<std::fs::File> {
        panic!("contract declarations only; never execute")
    }
}
#[path = CONFIG] mod config;
mod head {
    #[derive(Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    pub struct Head {
        pub version: u32, pub hub: String, pub revision: u64, pub snapshot: String,
    }
    pub struct ObservedHead { pub head: Head, pub token: String }
    pub struct HeadStore;
    impl HeadStore {
        pub fn new(_: &crate::config::Config) -> crate::error::Result<Self> {
            panic!("contract declarations only; never execute")
        }
        pub fn read(&self) -> crate::error::Result<Option<ObservedHead>> {
            panic!("contract declarations only; never execute")
        }
        pub fn compare_exchange(&self, _: Option<&ObservedHead>, _: &Head)
            -> crate::error::Result<bool> {
            panic!("contract declarations only; never execute")
        }
    }
    #[path = TESTS] mod tests;
}
"""
    for key, path in (("ERROR", "src/error.rs"), ("CONFIG", "src/config.rs"), ("TESTS", "src/head_tests.rs")):
        source = source.replace(key, json.dumps(str(root / path)))
    with tempfile.TemporaryDirectory(prefix="ownerless-head-contract-") as directory:
        scratch = Path(directory)
        harness = scratch / "contract.rs"
        harness.write_text(source)
        environment = dict(os.environ, CARGO_MANIFEST_DIR=str(root))
        subprocess.run(
            ["rustc", "--edition=2024", "--crate-name", "ownerless_head_contract_surface",
             "--test", "--emit=metadata", "-L", f"dependency={deps}", *externs,
             str(harness), "-o", str(scratch / "contract.rmeta")],
            cwd=root, env=environment, timeout=60, check=True,
        )
    print("Contract-surface Rust metadata check OK; no head backend assertions executed.")


if __name__ == "__main__":
    main()
