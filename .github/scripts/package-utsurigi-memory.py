"""Build the source ZIP reproducibly, or check that it matches the source tree."""
import argparse
import importlib.util
import io
from pathlib import Path
import stat
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "optional/utsurigi-memory"
ARCHIVE = SOURCE.with_suffix(".zip")


def package():
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for path in sorted(SOURCE.rglob("*")):
            relative = path.relative_to(SOURCE)
            if {"target", "__pycache__", ".git"} & set(relative.parts):
                continue
            if path.is_symlink():
                raise SystemExit(f"source package contains a symlink: {relative}")
            if not path.is_file():
                continue
            entry = zipfile.ZipInfo(f"utsurigi-memory/{relative.as_posix()}", (1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.compress_type = zipfile.ZIP_DEFLATED
            mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
            entry.external_attr = (stat.S_IFREG | mode) << 16
            archive.writestr(entry, path.read_bytes(), compresslevel=9)
    data = output.getvalue()
    sys.dont_write_bytecode = True
    spec = importlib.util.spec_from_file_location("archive_check", Path(__file__).with_name("check-utsurigi-memory.py"))
    checker = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checker)
    failures = checker.check(data)
    if failures:
        raise SystemExit("\n".join(failures))
    return data


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    data = package()
    if args.check:
        if not ARCHIVE.is_file() or ARCHIVE.read_bytes() != data:
            raise SystemExit("memory ZIP differs from source; run python3 .github/scripts/package-utsurigi-memory.py")
        print("utsurigi-memory source/ZIP: identical")
    else:
        ARCHIVE.write_bytes(data)
        print("utsurigi-memory ZIP rebuilt from source")
