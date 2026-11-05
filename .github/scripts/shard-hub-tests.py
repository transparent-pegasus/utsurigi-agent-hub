"""Shard the default Cargo test suite; run from the repository root.

Ignored tests are partitioned too and retain libtest's normal ignored semantics.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import tomllib


def validate_shard(shard, count):
    if count < 1 or not 0 <= shard < count:
        raise ValueError("require --shards > 0 and 0 <= --shard < --shards")


def parse_list(output):
    names = []
    lines = [line for line in output.splitlines() if line]
    if not lines or not re.fullmatch(r"\d+ tests?, 0 benchmarks", lines[-1]):
        raise ValueError("unsupported libtest enumeration (expected tests and no benchmarks)")
    for line in lines[:-1]:
        if not line.endswith(": test") or not line[:-6]:
            raise ValueError(f"unexpected libtest entry: {line!r}")
        names.append(line[:-6])
    if int(lines[-1].split()[0]) != len(names) or len(set(names)) != len(names):
        raise ValueError("incomplete or duplicate libtest enumeration")
    return sorted(names)


def partition(tests, count, isolated=()):
    validate_shard(0, count)
    names = sorted(tests)
    if not names or len(set(names)) != len(names):
        raise ValueError("empty or duplicate test identities")
    identities = {f"{kind}:{target}::{name}": (kind, target, name) for kind, target, name in names}
    if len(set(isolated)) != len(isolated) or any(identity not in identities for identity in isolated):
        raise ValueError("unknown or duplicate isolated test identities")
    dedicated = [identities[identity] for identity in isolated]
    remaining = [name for name in names if name not in dedicated]
    available = count - len(dedicated)
    if available < 0 or (remaining and not available):
        raise ValueError("require a shard for each isolated test and a shard for remaining tests")
    groups = [{name} for name in dedicated]
    groups.extend(set(remaining[index::available]) for index in range(available))
    if set.union(*groups) != set(names) or sum(map(len, groups)) != len(names):
        raise ValueError("test partition is incomplete or overlapping")
    return groups


def cargo_command(target, excluded):
    kind, name = target
    command = ["cargo", "test", "--locked", f"--{kind}", name, "--", "--test-threads=1", "--exact"]
    for test in sorted(excluded):
        command.extend(["--skip", test])
    return command


def run_case(test):
    kind, target, name = test
    command = cargo_command((kind, target), []) + [name]
    started = time.monotonic()
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    except OSError as error:
        result = subprocess.CompletedProcess(command, 1, f"sharder: {error}\n")
    return result, time.monotonic() - started


def parse_artifacts(output, package_id, expected):
    executables = {}
    for line in output.splitlines():
        message = json.loads(line)
        if message["reason"] == "compiler-artifact" and message["profile"]["test"]:
            target = (message["target"]["kind"][0], message["target"]["name"])
            if (message["package_id"] != package_id or target not in expected
                    or target in executables or not message["executable"]):
                raise ValueError(f"unexpected or duplicate test artifact: {target}")
            executables[target] = message["executable"]
    if set(executables) != expected:
        raise ValueError(f"test artifact coverage mismatch: expected {expected}, got {set(executables)}")
    return executables


def discover():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"], text=True))
    if len(metadata["packages"]) != 1:
        raise ValueError("update sharder before adding workspace packages")
    package = metadata["packages"][0]
    if metadata["workspace_default_members"] != [package["id"]]:
        raise ValueError("unexpected Cargo default package selection")
    manifest = tomllib.loads(Path(package["manifest_path"]).read_text())
    if any(not target.get("harness", True) for kind in ["bin", "test", "bench", "example"]
           for target in manifest.get(kind, [])):
        raise ValueError("custom harness targets require explicit sharder support")
    expected = set()
    for target in package["targets"]:
        if target["doctest"] or target["kind"] not in (["bin"], ["test"], ["bench"], ["example"], ["custom-build"]):
            raise ValueError("library/doc-test targets require explicit sharder support")
        if target["test"]:
            if target["kind"] not in (["bin"], ["test"]):
                raise ValueError("unsupported Cargo test target")
            expected.add((target["kind"][0], target["name"]))
    output = subprocess.check_output(
        ["cargo", "test", "--locked", "--no-run", "--message-format=json"], text=True)
    executables = parse_artifacts(output, package["id"], expected)
    return {target: parse_list(subprocess.check_output([executable, "--list"], text=True))
            for target, executable in sorted(executables.items())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shard", type=int)
    parser.add_argument("--shards", type=int, default=8)
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument("--isolate", action="append", default=[])
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.workers < 1:
        parser.error("require --workers > 0")
    if args.self_test:
        self_test()
        return 0
    if args.shard is None:
        parser.error("--shard is required")
    try:
        validate_shard(args.shard, args.shards)
    except ValueError as error:
        parser.error(str(error))
    tests = discover()
    try:
        groups = partition([(kind, target, name) for (kind, target), names in tests.items() for name in names],
                           args.shards, args.isolate)
    except ValueError as error:
        parser.error(str(error))
    print(f"Coverage: {sum(map(len, groups))} listed tests; complete/disjoint shard sizes {list(map(len, groups))}", flush=True)
    failed = 0
    cases = []
    for target, names in sorted(tests.items()):
        selected = {name for name in names if (*target, name) in groups[args.shard]}
        if not selected:
            continue
        print(f"Shard {args.shard}: {target[0]} {target[1]}: {len(selected)}/{len(names)} tests", flush=True)
        if args.dry_run:
            for name in sorted(selected):
                print(f"  {target[0]}:{target[1]}::{name}")
        elif args.workers == 1:
            result = subprocess.run(cargo_command(target, set(names) - selected))
            failed = failed or result.returncode
        else:
            cases.extend((*target, name) for name in sorted(selected))
    if cases:
        with ThreadPoolExecutor(max_workers=args.workers) as executor:
            futures = {executor.submit(run_case, test): test for test in cases}
            try:
                for future in as_completed(futures):
                    kind, target, name = futures[future]
                    result, elapsed = future.result()
                    print(f"Shard {args.shard}: {kind}:{target}::{name} ({elapsed:.2f}s, exit {result.returncode})", flush=True)
                    print(result.stdout, end="", flush=True)
                    failed = failed or result.returncode
            except BaseException:
                for future in futures:
                    future.cancel()
                raise
    return failed


def self_test():
    assert parse_list("a: test\nab: test\nnested::a: test\n\n3 tests, 0 benchmarks\n") == ["a", "ab", "nested::a"]
    assert parse_list("\n0 tests, 0 benchmarks\n") == []
    for output in ["", "a: test\n2 tests, 0 benchmarks", "a: benchmark\n0 tests, 1 benchmark", "a: test\na: test\n2 tests, 0 benchmarks"]:
        try:
            parse_list(output)
        except ValueError:
            pass
        else:
            raise AssertionError(f"accepted invalid enumeration: {output!r}")
    tests = [("test", f"target-{i % 3}", f"case-{i}") for i in range(61)]
    tests += [("bin", "same-target", "same-test"), ("test", "same-target", "same-test")]
    groups = partition(tests, 8)
    assert groups == partition(reversed(tests), 8)
    assert set.union(*groups) == set(tests)
    assert sum(map(len, groups)) == len(tests)
    assert max(map(len, groups)) - min(map(len, groups)) <= 1
    for shard, count in [(-1, 8), (8, 8), (0, 0), (0, -1)]:
        try:
            validate_shard(shard, count)
        except ValueError:
            pass
        else:
            raise AssertionError("accepted invalid shard parameters")
    try:
        partition(tests + tests[:1], 8)
    except ValueError:
        pass
    else:
        raise AssertionError("accepted duplicate test identities")
    artifact = {"reason": "compiler-artifact", "profile": {"test": True}, "package_id": "fixture",
                "target": {"kind": ["test"], "name": "fixture"}, "executable": "/fixture"}
    expected = {("test", "fixture")}
    assert parse_artifacts(json.dumps(artifact), "fixture", expected) == {("test", "fixture"): "/fixture"}
    for messages in [[], [artifact, artifact], [artifact | {"package_id": "other"}],
                     [artifact | {"executable": None}], [artifact | {"target": {"kind": ["test"], "name": "other"}}]]:
        try:
            parse_artifacts("\n".join(map(json.dumps, messages)), "fixture", expected)
        except ValueError:
            pass
        else:
            raise AssertionError("accepted missing, duplicate or unexpected Cargo artifacts")

    with tempfile.TemporaryDirectory(prefix="hub-shard-self-test-") as directory:
        root = Path(directory)
        (root / "src").mkdir()
        manifest = '[package]\nname = "shard-fixture"\nversion = "0.0.0"\nedition = "2024"\n'
        (root / "Cargo.toml").write_text(manifest)
        (root / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "shard-fixture"\nversion = "0.0.0"\n')
        (root / "src/main.rs").write_text('''fn main() {}
#[test] fn a() {}
#[test] fn ab() {}
mod nested { #[test] fn a() {} }
#[test] #[ignore] fn ignored() { panic!("must remain ignored"); }
#[test] fn failure() { panic!("intentional failure"); }
''')
        import os
        environment = os.environ | {"CARGO_TARGET_DIR": str(root / "target")}

        def run(arguments):
            return subprocess.run(arguments, cwd=root, env=environment, text=True, capture_output=True)

        script = str(Path(__file__).resolve())
        for arguments in [["--shard", "-1"], ["--shard", "8"], ["--shard", "0", "--shards", "0"], []]:
            result = run([sys.executable, script, *arguments])
            assert result.returncode == 2, result.stderr
        result = run([sys.executable, script, "--shard", "0", "--dry-run"])
        assert result.returncode == 0, result.stderr
        result = run(cargo_command(("bin", "shard-fixture"), ["a", "failure"]))
        assert result.returncode == 0, result.stdout + result.stderr
        assert "test ab ... ok" in result.stdout and "test nested::a ... ok" in result.stdout
        assert "1 ignored" in result.stdout and "2 passed" in result.stdout
        result = run([sys.executable, script, "--shard", "2"])
        assert result.returncode == 101 and "test failure ... FAILED" in result.stdout, result.stdout + result.stderr
        (root / "tests").mkdir()
        (root / "tests/later.rs").write_text("#[test] fn later_target() {}\n")
        result = run([sys.executable, script, "--shard", "0", "--shards", "1"])
        assert result.returncode == 101 and "test later_target ... ok" in result.stdout, result.stdout + result.stderr
        (root / "Cargo.toml").write_text(manifest + '\n[[bin]]\nname = "shard-fixture"\npath = "src/main.rs"\nharness = false\n')
        result = run([sys.executable, script, "--shard", "0", "--dry-run"])
        assert result.returncode != 0 and "custom harness" in result.stderr, result.stderr
        (root / "Cargo.toml").write_text(manifest)
        (root / "src/lib.rs").write_text("pub fn library() {}\n")
        result = run([sys.executable, script, "--shard", "0", "--dry-run"])
        assert result.returncode != 0 and "library/doc-test" in result.stderr, result.stderr
    print("sharder self-test: complete/disjoint/deterministic partition, invalid inputs/artifacts, exact skips, ignored tests, failure propagation/continuation and harness guards passed")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        print(f"sharder command failed: {error}", file=sys.stderr)
        raise SystemExit(error.returncode)
    except (OSError, ValueError) as error:
        raise SystemExit(f"sharder: {error}")
