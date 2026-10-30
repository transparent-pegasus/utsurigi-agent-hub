import io
from pathlib import Path, PurePosixPath
import re
import stat
import sys
import zipfile


def check(data):
    failures = []
    seen = set()
    files = set()
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        for item in archive.infolist():
            name = item.filename
            path = PurePosixPath(name)
            parts = name.rstrip("/").split("/")
            reasons = set()
            if (parts[0] != "utsurigi-memory" or any(part in {"", ".", ".."} for part in parts)
                    or "\\" in name or ":" in name or "\x00" in item.orig_filename):
                reasons.add("unsafe entry path")
            if name.rstrip("/") in seen:
                reasons.add("duplicate entry")
            seen.add(name.rstrip("/"))
            mode = item.external_attr >> 16
            if stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR):
                reasons.add("symlink or special file")
            if {"target", "build", "dist", "node_modules", "__pycache__", ".git", ".venv"} & set(parts):
                reasons.add("build or repository output")
            if any(part.lower().startswith(".env") or Path(part).stem.lower() in {
                ".aws", ".ssh", ".gnupg", ".netrc", "credentials", "secrets", "id_rsa", "id_ed25519"
            } for part in parts):
                reasons.add("credential material")
            if not item.is_dir():
                files.add(name)
                content = archive.read(item)
                # shortcut: explicit text formats; extend when the package adds a source format.
                if path.suffix.lower() not in {
                    ".rs", ".toml", ".lock", ".md", ".json", ".sha256", ".sh", ".txt", ".py", ".yaml", ".yml"
                } and not (not path.suffix and content.startswith(b"#!")):
                    reasons.add("binary, database, or model weights")
                if content.startswith((b"\x7fELF", b"MZ", b"\xcf\xfa\xed\xfe", b"!<arch>")) or re.search(
                    rb"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]", content
                ):
                    reasons.add("binary content")
                try:
                    content.decode("utf-8")
                except UnicodeDecodeError:
                    reasons.add("non-UTF-8 content")
                if mode & 0o111 and not content.startswith(b"#!"):
                    reasons.add("executable without script header")
                if re.search(rb"-----BEGIN [A-Z0-9 ]*PRIVATE KEY|(?:AKIA|ASIA)[0-9A-Z]{16}", content):
                    reasons.add("credential material")
                if re.search(rb"/cursor/stores/|/(?:home|Users)/[^/\s\"']+|/root/|/mnt/|\b[^\s]+'s personal memory store", content):
                    reasons.add("personal deployment identifier")
            failures.extend(f"{name!r}: {reason}" for reason in sorted(reasons))
    for name in sorted({"utsurigi-memory/Cargo.toml", "utsurigi-memory/Cargo.lock", "utsurigi-memory/src/main.rs", "utsurigi-memory/tests/cli.rs"} - files):
        failures.append(f"missing required file: {name}")
    return failures


def self_test():
    files = {
        "utsurigi-memory/Cargo.toml": b'[package]\nname = "fixture"\n',
        "utsurigi-memory/Cargo.lock": b"version = 4\n",
        "utsurigi-memory/src/main.rs": b"fn main() {}\n",
        "utsurigi-memory/tests/cli.rs": b"#[test] fn fixture() {}\n",
        "utsurigi-memory/tests/notes.txt": "日本語 café 🦀\n".encode(),
        "utsurigi-memory/model/tokenizer.json": '{"token": "記憶"}'.encode(),
    }

    def fixture(extra=None, mode=None, omit=None):
        output = io.BytesIO()
        with zipfile.ZipFile(output, "w") as archive:
            for name, content in files.items():
                if name != omit:
                    archive.writestr(name, content)
            if extra:
                name, content = extra
                entry = zipfile.ZipInfo(name)
                if mode is not None:
                    entry.create_system = 3
                    entry.external_attr = mode << 16
                archive.writestr(entry, content)
        return output.getvalue()

    assert not check(fixture()), "Unicode source package must pass"
    assert not check(fixture(("utsurigi-memory/utsurigi-memory", b"#!/bin/bash\nexit 0\n"), stat.S_IFREG | 0o755))
    rejected = [
        ("/absolute.txt", b"text", "unsafe entry path"),
        ("utsurigi-memory/../../escape.txt", b"text", "unsafe entry path"),
        ("C:\\escape.txt", b"text", "unsafe entry path"),
        ("utsurigi-memory/..\\escape.txt", b"text", "unsafe entry path"),
        ("other/src/main.rs", b"text", "unsafe entry path"),
        ("utsurigi-memory/target/debug/cache.txt", b"text", "build or repository output"),
        ("utsurigi-memory/node_modules/cache.txt", b"text", "build or repository output"),
        ("utsurigi-memory/__pycache__/cache.txt", b"text", "build or repository output"),
        ("utsurigi-memory/.git/config.txt", b"text", "build or repository output"),
        ("utsurigi-memory/app.txt", b"\x7fELFfixture", "binary content"),
        ("utsurigi-memory/app.txt", b"MZfixture", "binary content"),
        ("utsurigi-memory/runtime.so.1", b"text", "binary, database, or model weights"),
        ("utsurigi-memory/model/model.onnx", b"text", "binary, database, or model weights"),
        ("utsurigi-memory/model/weights.safetensors", b"text", "binary, database, or model weights"),
        ("utsurigi-memory/memories.sqlite", b"text", "binary, database, or model weights"),
        ("utsurigi-memory/data.txt", b"SQLite format 3\x00fixture", "binary content"),
        ("utsurigi-memory/data.txt", b"\xff\xfe", "non-UTF-8 content"),
        ("utsurigi-memory/.env", b"TOKEN=fixture", "credential material"),
        ("utsurigi-memory/.env/notes.txt", b"TOKEN=fixture", "credential material"),
        ("utsurigi-memory/.aws/config.json", b"text", "credential material"),
        ("utsurigi-memory/.ssh/config.txt", b"text", "credential material"),
        ("utsurigi-memory/credentials.json", b"text", "credential material"),
        ("utsurigi-memory/private.pem", b"text", "binary, database, or model weights"),
        ("utsurigi-memory/note.txt", b"-----BEGIN PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN RSA PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN OPENSSH PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN EC PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN ENCRYPTED PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN DSA PRIVATE KEY-----", "credential material"),
        ("utsurigi-memory/note.txt", b"-----BEGIN PGP PRIVATE KEY BLOCK-----", "credential material"),
        ("utsurigi-memory/note.txt", b"AKIA" + b"A" * 16, "credential material"),
        ("utsurigi-memory/note.txt", b"ASIA" + b"A" * 16, "credential material"),
        ("utsurigi-memory/note.txt", b"/home/example-user/private", "personal deployment identifier"),
        ("utsurigi-memory/note.txt", b"/Users/example-user/private", "personal deployment identifier"),
        ("utsurigi-memory/note.txt", b"/root/fixture", "personal deployment identifier"),
        ("utsurigi-memory/note.txt", b"/mnt/fixture", "personal deployment identifier"),
        ("utsurigi-memory/note.txt", b"/cursor/stores/private", "personal deployment identifier"),
        ("utsurigi-memory/note.txt", b"Example's personal memory store", "personal deployment identifier"),
    ]
    missing = []

    def require_reason(data, expected, case):
        actual = check(data)
        if expected not in actual:
            missing.append(f"{case}: expected {expected!r}, got {actual!r}")

    for name, content, reason in rejected:
        require_reason(fixture((name, content)), f"{name!r}: {reason}", (name, content))
    for mode in (stat.S_IFLNK | 0o777, stat.S_IFIFO | 0o600):
        require_reason(fixture(("utsurigi-memory/link.txt", b"target"), mode), "'utsurigi-memory/link.txt': symlink or special file", mode)
    require_reason(fixture(("utsurigi-memory/app.txt", b"text"), stat.S_IFREG | 0o755), "'utsurigi-memory/app.txt': executable without script header", "executable")
    require_reason(fixture(("utsurigi-memory/src/main.rs/", b"")), "'utsurigi-memory/src/main.rs/': duplicate entry", "duplicate")
    for name in ("utsurigi-memory/Cargo.toml", "utsurigi-memory/Cargo.lock", "utsurigi-memory/src/main.rs", "utsurigi-memory/tests/cli.rs"):
        require_reason(fixture(omit=name), f"missing required file: {name}", "missing required file")
    empty = io.BytesIO()
    with zipfile.ZipFile(empty, "w"):
        pass
    for name in files:
        if name in {"utsurigi-memory/Cargo.toml", "utsurigi-memory/Cargo.lock", "utsurigi-memory/src/main.rs", "utsurigi-memory/tests/cli.rs"}:
            require_reason(empty.getvalue(), f"missing required file: {name}", "empty archive")
    assert not missing, "\n".join(missing)
    print(f"archive checker self-tests: {len(rejected) + 14} passed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) == 2:
        failures = check(Path(sys.argv[1]).read_bytes())
        if failures:
            raise SystemExit("\n".join(failures))
        print("utsurigi-memory archive: OK")
    else:
        raise SystemExit("usage: check-utsurigi-memory.py ARCHIVE | --self-test")
