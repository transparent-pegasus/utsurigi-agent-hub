"""Public CLI regressions; run with python3 .github/scripts/test-shard-hub-tests.py."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SHARDER = Path(__file__).resolve().with_name("shard-hub-tests.py")
IDENTITIES = {
    "bin:probe::a", "bin:probe::ab", "bin:probe::fail_first",
    "bin:probe::ignored", "bin:probe::nested::a", "bin:probe::z_after_failure",
    "test:probe::a", "test:probe::ab", "test:probe::z_later_target",
}
RUNNABLE = {
    "bin-a", "bin-ab", "bin-fail_first", "bin-nested-a", "bin-z_after_failure",
    "test-a", "test-ab", "test-z_later_target",
}
PROBE = r'''
use std::{env, fs, io::Write, path::PathBuf, process, thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn record(root: &PathBuf, label: &str, phase: &str) {
    let mut file = fs::OpenOptions::new().write(true).create_new(true)
        .open(root.join(format!("{label}.{phase}"))).unwrap();
    writeln!(file, "{} {}", process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap();
}

fn probe(label: &str) {
    assert_eq!(env::var("CARGO_MANIFEST_DIR").unwrap(), env!("CARGO_MANIFEST_DIR"));
    let root = PathBuf::from(env::var_os("UAH_PROBE_DIR").unwrap());
    record(&root, label, "start");
    if env::var("UAH_REQUIRE_OVERLAP").as_deref() == Ok("1") {
        let deadline = Instant::now() + Duration::from_secs(15);
        // A rendezvous proves overlap even when Cargo startup is slow.
        while fs::read_dir(&root).unwrap().filter(|entry|
            entry.as_ref().unwrap().path().extension().is_some_and(|ext| ext == "start")
        ).count() < 2 {
            assert!(Instant::now() < deadline, "test processes did not overlap");
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(100));
    }
    record(&root, label, "end");
}
'''


class SharderCLI(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory(prefix="test-shard-hub-")
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name)
        (cls.root / "src").mkdir()
        (cls.root / "tests").mkdir()
        (cls.root / "Cargo.toml").write_text(
            '[package]\nname = "probe"\nversion = "0.0.0"\nedition = "2024"\n'
        )
        (cls.root / "Cargo.lock").write_text(
            'version = 4\n[[package]]\nname = "probe"\nversion = "0.0.0"\n'
        )
        (cls.root / "probe.rs").write_text(PROBE)
        (cls.root / "src/main.rs").write_text(r'''
include!("../probe.rs");
fn main() {}
#[test] fn a() { probe("bin-a"); }
#[test] fn ab() { probe("bin-ab"); }
#[test] fn fail_first() {
    probe("bin-fail_first");
    assert!(env::var("UAH_FAIL_CASE").as_deref() != Ok("1"),
        "intentional sharder fixture failure");
}
#[test] #[ignore] fn ignored() { probe("bin-ignored"); panic!("must remain ignored"); }
mod nested { #[test] fn a() { super::probe("bin-nested-a"); } }
#[test] fn z_after_failure() { probe("bin-z_after_failure"); }
''')
        (cls.root / "tests/probe.rs").write_text(r'''
include!("../probe.rs");
#[test] fn a() { probe("test-a"); }
#[test] fn ab() { probe("test-ab"); }
#[test] fn z_later_target() { probe("test-z_later_target"); }
''')

    def invoke(self, *arguments, environment=None):
        with tempfile.TemporaryDirectory(prefix="events-", dir=self.root) as directory:
            env = os.environ | {
                "CARGO_TARGET_DIR": str(self.root / "target"),
                "UAH_PROBE_DIR": directory,
                "UAH_FAIL_CASE": "",
                "UAH_REQUIRE_OVERLAP": "",
            } | (environment or {})
            result = subprocess.run(
                [sys.executable, str(SHARDER), *arguments], cwd=self.root, env=env,
                text=True, capture_output=True, timeout=60,
            )
            events = {path.name: tuple(map(int, path.read_text().split()))
                      for path in Path(directory).iterdir()}
        return result, events

    def assert_completed(self, events, expected=RUNNABLE):
        self.assertEqual(set(events), {f"{label}.{phase}"
                                      for label in expected for phase in ("start", "end")})
        for label in expected:
            start_pid, start = events[f"{label}.start"]
            end_pid, end = events[f"{label}.end"]
            self.assertEqual(start_pid, end_pid)
            self.assertLess(start, end)

    def partitions(self, count, isolates=()):
        arguments = [argument for identity in isolates for argument in ("--isolate", identity)]
        groups = []
        for shard in range(count):
            result, events = self.invoke("--shard", str(shard), "--shards", str(count),
                                         "--dry-run", *arguments)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(events, "dry-run executed a test")
            identities = [line.strip() for line in result.stdout.splitlines()
                          if line.startswith(("  bin:", "  test:"))]
            self.assertEqual(len(identities), len(set(identities)), result.stdout)
            groups.append(set(identities))
        self.assertEqual(set.union(*groups), IDENTITIES)
        self.assertEqual(sum(map(len, groups)), len(IDENTITIES), "overlapping shards")
        return groups

    def test_default_execution_preserves_ignored_and_cargo_environment(self):
        result, events = self.invoke("--shard", "0", "--shards", "1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_completed(events)
        self.assertIn("1 ignored", result.stdout)

    def test_explicit_single_worker_preserves_behavior(self):
        result, events = self.invoke("--shard", "0", "--shards", "1", "--workers", "1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_completed(events)
        self.assertIn("1 ignored", result.stdout)

    def test_two_workers_overlap_in_separate_processes_and_stay_bounded(self):
        result, events = self.invoke("--shard", "0", "--shards", "1", "--workers", "2",
                                     environment={"UAH_REQUIRE_OVERLAP": "1"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assert_completed(events)
        self.assertEqual(len({events[f"{label}.start"][0] for label in RUNNABLE}), len(RUNNABLE),
                         "each test must run in its own process")
        active = peak = 0
        timeline = [(stamp, 1 if name.endswith(".start") else -1)
                    for name, (_, stamp) in events.items()]
        for _, change in sorted(timeline):
            active += change
            self.assertGreaterEqual(active, 0)
            peak = max(peak, active)
        self.assertEqual(active, 0)
        self.assertEqual(peak, 2, "require actual overlap and at most two active test processes")
        self.assertIn("1 ignored", result.stdout)

    def test_isolation_executes_only_the_exact_target_and_case(self):
        for identity, label in (("bin:probe::a", "bin-a"), ("test:probe::a", "test-a")):
            with self.subTest(identity=identity):
                result, events = self.invoke("--shard", "0", "--shards", "3", "--workers", "2",
                                             "--isolate", identity)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_completed(events, {label})

    def test_failure_is_visible_and_all_remaining_cases_finish(self):
        result, events = self.invoke("--shard", "0", "--shards", "1", "--workers", "2",
                                     environment={"UAH_FAIL_CASE": "1"})
        self.assertIn("intentional sharder fixture failure", result.stdout + result.stderr)
        self.assertNotEqual(result.returncode, 0)
        self.assert_completed(events)

    def test_partition_without_isolation_is_complete_disjoint_and_deterministic(self):
        for count in (1, 3, 12):
            with self.subTest(shards=count):
                groups = self.partitions(count)
                self.assertEqual(groups, [set(sorted(IDENTITIES)[shard::count])
                                          for shard in range(count)])
        self.assertEqual(self.partitions(3), self.partitions(3))

    def test_exact_isolation_has_dedicated_earliest_shards(self):
        identity = "test:probe::a"
        groups = self.partitions(3, [identity])
        self.assertEqual(groups[0], {identity})
        remaining = sorted(IDENTITIES - {identity})
        self.assertEqual(groups[1:], [set(remaining[::2]), set(remaining[1::2])])
        self.assertEqual(groups, self.partitions(3, [identity]))
        isolates = ["test:probe::a", "bin:probe::ab"]
        groups = self.partitions(4, isolates)
        self.assertEqual({frozenset(group) for group in groups[:2]},
                         {frozenset({identity}) for identity in isolates})
        remaining = sorted(IDENTITIES - set(isolates))
        self.assertEqual(groups[2:], [set(remaining[::2]), set(remaining[1::2])])

    def test_unknown_duplicate_isolates_and_impossible_shard_counts_are_rejected(self):
        invalid = [
            (3, ["test:probe::missing"]),
            (3, ["probe::a"]),
            (3, ["test:probe::a", "test:probe::a"]),
            (1, ["test:probe::a", "bin:probe::a"]),
            (1, ["test:probe::a"]),
        ]
        for count, isolates in invalid:
            with self.subTest(shards=count, isolates=isolates):
                arguments = [argument for identity in isolates
                             for argument in ("--isolate", identity)]
                result, events = self.invoke("--shard", "0", "--shards", str(count),
                                             "--dry-run", *arguments)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertNotIn("unrecognized arguments", result.stderr)
                self.assertFalse(events)

    def test_invalid_workers_are_rejected(self):
        for workers in ("0", "-1", "many", "1.5"):
            with self.subTest(workers=workers):
                result, events = self.invoke("--shard", "0", "--shards", "1", "--workers", workers)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertNotIn("unrecognized arguments", result.stderr)
                self.assertFalse(events)


if __name__ == "__main__":
    unittest.main(verbosity=2)
