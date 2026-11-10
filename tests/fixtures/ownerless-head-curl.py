#!/usr/bin/python3
"""Offline curl boundary for head and small-object tests; this program has no network path.

Installed as <case>/bin/curl. All state is private, synthetic case data.
Accept curl's config grammar rather than one serializer's whitespace/quotes.
The head URL uses the single-head remote state. Any other key under PREFIX is
an object in remote["objects"]: {key: {"etag": ..., "body_hex": ...}}.
Run --self-test to check the parser and conditional object-store fixture.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import sys
import time


ALIASES = {
    "X": "request", "D": "dump-header", "o": "output", "w": "write-out",
    "H": "header", "u": "user", "T": "upload-file", "d": "data",
    "K": "config", "s": "silent", "S": "show-error", "f": "fail",
    "i": "include", "I": "head", "q": "disable",
}
FLAGS = {
    "silent", "show-error", "fail", "fail-with-body", "include", "head",
    "disable", "globoff", "no-progress-meter", "http1.1", "http2",
    "location", "compressed", "get", "no-buffer",
}
PREFIX = ("https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com"
          "/ownerless-head-fixture/shared/prefix/")
HEAD_NAME = "uah-head.json"
# The product's RLIMIT_FSIZE also binds this child: keep large bodies out of
# calls.jsonl and remote.json, recording their size and digest instead.
INLINE_LIMIT = 4 * 1024


def decode_value(text):
    text = text.strip()
    if not text.startswith('"'):
        return text
    result = []
    index = 1
    escapes = {"n": "\n", "r": "\r", "t": "\t", "v": "\v"}
    while index < len(text):
        char = text[index]
        index += 1
        if char == '"':
            rest = text[index:].strip()
            if rest and not rest.startswith("#"):
                raise ValueError("trailing config text")
            return "".join(result)
        if char == "\\":
            if index == len(text):
                raise ValueError("unfinished config escape")
            char = text[index]
            index += 1
            char = escapes.get(char, char)
        result.append(char)
    raise ValueError("unterminated config string")


def parse_config(document):
    options = []
    for line in document.splitlines():
        line = line.lstrip()
        if not line or line.startswith("#"):
            continue
        match = re.fullmatch(r"(?:--)?([A-Za-z0-9.-]+)(.*)", line)
        if not match:
            raise ValueError("invalid config option")
        key, rest = match.groups()
        if rest and not rest[0].isspace() and rest[0] not in "=:":
            raise ValueError("missing config separator")
        rest = rest.lstrip()
        if rest.startswith(("=", ":")):
            rest = rest[1:].lstrip()
        options.append((ALIASES.get(key, key), decode_value(rest)))
    return options


def parse_request(arguments, document):
    if not arguments or arguments[0] != "-q":
        raise ValueError("-q must suppress ambient curl config first")
    options = []
    index = 0
    used_stdin = False
    while index < len(arguments):
        argument = arguments[index]
        index += 1
        if argument.startswith("--"):
            key, separator, value = argument[2:].partition("=")
        elif argument.startswith("-"):
            short = argument[1:]
            if len(short) > 1 and all(ALIASES.get(c, c) in FLAGS for c in short):
                options.extend((ALIASES.get(c, c), "") for c in short)
                continue
            key, separator, value = short[:1], "", short[1:]
            separator = bool(value)
        else:
            options.append(("url", argument))
            continue
        key = ALIASES.get(key, key)
        if key not in FLAGS and not separator:
            if index == len(arguments):
                raise ValueError("missing option argument")
            value = arguments[index]
            index += 1
        if key == "config":
            if value != "-" or used_stdin:
                raise ValueError("fixture only accepts one stdin config")
            options.extend(parse_config(document))
            used_stdin = True
        else:
            options.append((key, value))
    if not used_stdin:
        raise ValueError("credentials must be delivered in stdin config")
    return options


def one(options, name, default=None):
    values = [value for key, value in options if key == name]
    if len(values) > 1:
        raise ValueError("duplicate singleton option: " + name)
    return values[0] if values else default


def private_write(path, contents):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(contents)


def append_call(root, call):
    descriptor = os.open(root / "calls.jsonl", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(descriptor, "w") as stream:
        stream.write(json.dumps(call) + "\n")


def file_details(path):
    metadata = os.lstat(path)
    return {
        "regular": stat.S_ISREG(metadata.st_mode),
        "mode": stat.S_IMODE(metadata.st_mode),
        "parent_mode": stat.S_IMODE(os.stat(Path(path).parent).st_mode),
    }


def upload_body(options, files):
    for key in ("upload-file", "data-binary", "data-raw", "data"):
        value = one(options, key)
        if value is None:
            continue
        if key == "upload-file" or (key != "data-raw" and value.startswith("@")):
            path = value if key == "upload-file" else value[1:]
            if path == "-":
                raise ValueError("stdin is already the curl config stream")
            files.append(file_details(path))
            return Path(path).read_bytes()
        return value.encode()
    return b""


def object_key(options):
    """None selects the single-head state; anything else is an object key."""
    url = one(options, "url")
    if url is None or url.endswith("/" + HEAD_NAME):
        return None
    if not url.startswith(PREFIX):
        raise ValueError("URL outside the fixture prefix")
    return url[len(PREFIX):]


def put_status(headers, etag):
    """Conditional PUT outcome against the current ETag, or None when absent."""
    if "if-match" in headers and "if-none-match" in headers:
        return 400
    if headers.get("if-none-match") == ["*"] and "if-match" not in headers:
        return 412 if etag is not None else 200
    if len(headers.get("if-match", [])) == 1 and "if-none-match" not in headers:
        return 200 if etag is not None and headers["if-match"] == [etag] else 412
    return 400  # An unconditional PUT is never accepted by this fixture.


def stored_body(stored):
    if "body_hex" not in stored:
        raise ValueError("large uploaded bodies are recorded by digest only")
    return bytes.fromhex(stored["body_hex"])


def request(root, arguments, document):
    options = parse_request(arguments, document)
    scenario = json.loads((root / "fixture.json").read_text())
    remote_path = root / "remote.json"
    remote = json.loads(remote_path.read_text())
    files = []
    body = upload_body(options, files)
    if one(options, "upload-file") is not None:
        default_method = "PUT"
    elif any(key in ("data", "data-binary", "data-raw") for key, _ in options):
        default_method = "POST"
    elif one(options, "head") is not None:
        default_method = "HEAD"
    else:
        default_method = "GET"
    method = one(options, "request", default_method).upper()
    headers = {}
    for key, value in options:
        if key == "header":
            name, separator, content = value.partition(":")
            if not separator:
                raise ValueError("invalid request header")
            headers.setdefault(name.lower(), []).append(content.strip())

    inline = len(body) <= INLINE_LIMIT
    object_name = object_key(options)
    call = {
        "argv": arguments, "options": options, "method": method, "key": object_name,
        "headers": headers, "body": body.decode("utf-8", "replace") if inline else None,
        "body_hex": body.hex() if inline else None, "body_size": len(body),
        "body_sha256": hashlib.sha256(body).hexdigest(),
        "environment": dict(os.environ), "files": files,
    }
    overrides = scenario.get("get", {})
    if method == "GET" and remote.get("put_seen"):
        overrides = {**overrides, **scenario.get("after_put_get", {})}
    if object_name is not None:
        objects = remote.setdefault("objects", {})
        stored = objects.get(object_name)
        etag = stored["etag"] if stored else None
        if method == "GET":
            fresh = etag is not None and headers.get("if-none-match") == [etag]
            status = overrides.get("status", 304 if fresh else 200 if stored else 404)
            if "body" in overrides:
                response = overrides["body"]
            else:
                response = "missing" if stored is None else b"" if fresh else stored_body(stored)
            etags = overrides.get("etags", [etag] if stored else [])
        elif method in ("PUT", "DELETE"):
            overrides = scenario.get(method.lower(), {})
            committed = overrides.get("commit_before_error", False)
            if method == "PUT":
                remote["put_seen"] = True
                status = overrides.get("status", put_status(headers, etag))
                if (200 <= status < 300) or committed:
                    generation = remote.get("generation", 1) + 1
                    remote["generation"] = generation
                    objects[object_name] = {"etag": f'"new.etag/+==:{generation}"'}
                    objects[object_name].update(
                        {"body_hex": body.hex()} if inline
                        else {"body_size": len(body), "body_sha256": call["body_sha256"]})
            else:
                status = overrides.get("status", 204 if stored else 404)
                if stored and ((200 <= status < 300) or committed):
                    del objects[object_name]
            private_write(remote_path, json.dumps(remote).encode())
            response, etags = overrides.get("body", ""), []
        else:
            raise ValueError("only GET, conditional PUT and DELETE are supported for objects")
    elif method == "GET":
        status = overrides.get("status", 200 if remote["exists"] else 404)
        response = overrides.get("body", remote.get("raw_body"))
        if response is None:
            response = json.dumps(remote.get("head")) if remote["exists"] else "missing"
        etags = overrides.get("etags", [remote["etag"]] if remote["exists"] else [])
    elif method == "PUT":
        overrides = scenario.get("put", {})
        remote["put_seen"] = True
        status = overrides.get(
            "status", put_status(headers, remote["etag"] if remote["exists"] else None))
        if (200 <= status < 300) or overrides.get("commit_before_error", False):
            generation = remote.get("generation", 1) + 1
            remote.update(exists=True, head=json.loads(body), generation=generation,
                          etag=f'"new.etag/+==:{generation}"')
            remote.pop("raw_body", None)
        if "replace_after" in overrides:
            remote.update(exists=True, head=overrides["replace_after"])
        private_write(remote_path, json.dumps(remote).encode())
        response, etags = overrides.get("body", ""), []
    else:
        raise ValueError("only GET and conditional PUT are supported for the head")

    if "body_bytes" in overrides:
        response = "x" * overrides["body_bytes"]
    if isinstance(response, str):
        response = response.encode()
    header_bytes = overrides.get("raw_headers")
    if header_bytes is None:
        header_bytes = f"HTTP/1.1 {status} Fixture\r\nContent-Length: {len(response)}\r\n"
        header_bytes += "".join(f"ETag: {etag}\r\n" for etag in etags)
        header_bytes += "\r\n"
    header_bytes = header_bytes.encode()
    if "header_bytes" in overrides:
        header_bytes = (b"HTTP/1.1 200 OK\r\nETag: " + remote["etag"].encode()
                        + b"\r\nX-Flood: " + b"x" * overrides["header_bytes"] + b"\r\n\r\n")

    for key, contents in (("output", response), ("dump-header", header_bytes)):
        path = one(options, key)
        if path and path != "-":
            # Emulate curl's ordinary file creation; do not make product files private for it.
            with open(path, "wb") as stream:
                stream.write(contents)
            call["files"].append(file_details(path))
        elif key == "output":
            sys.stdout.buffer.write(contents)
        elif one(options, "include") is not None:
            sys.stdout.buffer.write(contents)

    call["status"] = status
    append_call(root, call)
    maximum = float(one(options, "max-time", "120"))
    delay = overrides.get("delay_seconds", 0)
    if delay:
        time.sleep(min(delay, maximum))
        if delay >= maximum:
            return 28

    for name, stream in (("stdout_bytes", sys.stdout.buffer), ("stderr_bytes", sys.stderr.buffer)):
        remaining = overrides.get(name, 0)
        block = b"x" * 8192
        while remaining:
            chunk = block[:min(len(block), remaining)]
            try:
                stream.write(chunk)
                stream.flush()
            except BrokenPipeError:
                return 23
            remaining -= len(chunk)
    sys.stderr.write(overrides.get("stderr", ""))
    write_out = one(options, "write-out", "")
    write_out = write_out.replace("%{http_code}", f"{status:03}").replace("%{response_code}", f"{status:03}")
    write_out = write_out.replace("%{size_download}", str(len(response)))
    sys.stdout.write(write_out)
    failed_http = status >= 400 and any(key in ("fail", "fail-with-body") for key, _ in options)
    return overrides.get("exit", 22 if failed_http else 0)


def self_test():
    import tempfile
    import unittest
    from unittest.mock import patch
    import io

    class FixtureChecks(unittest.TestCase):
        def test_config_separators_aliases_and_escaping(self):
            want = [("url", "https://fixture.invalid/p"), ("user", 'key"\\:secret\\"'),
                    ("header", 'If-Match: "opaque/+=="'), ("max-time", "1"), ("silent", "")]
            document = '# comment\nurl:https://fixture.invalid/p\nu = "key\\\"\\\\:secret\\\\\\\""\n--header "If-Match: \\\"opaque/+==\\\""\nmax-time=1\nsilent\n'
            self.assertEqual(parse_config(document), want)
            self.assertEqual(parse_request(["-q", "-K", "-"], document)[1:], want)
            self.assertEqual(decode_value('"tab\\tline\\nend"'), "tab\tline\nend")

        def test_invalid_config_and_ambient_config(self):
            for document in ('header = "unterminated', 'url = "x" trailing'):
                with self.assertRaises(ValueError):
                    parse_config(document)
            with self.assertRaises(ValueError):
                parse_request(["--config", "-"], "")
            with self.assertRaises(ValueError):
                parse_request(["-q", "-K", "/real/config"], "")

        def test_executable_stdin_file_upload_and_lost_response(self):
            import subprocess
            with tempfile.TemporaryDirectory(prefix="ownerless-head-executable-check-") as directory:
                root = Path(directory)
                (root / "bin").mkdir()
                executable = root / "bin/curl"
                private_write(executable, Path(__file__).read_bytes())
                executable.chmod(0o700)
                private_write(root / "fixture.json", b'{"put":{"status":0,"exit":56,"commit_before_error":true}}')
                private_write(root / "remote.json", b'{"exists":true,"etag":"\\\"first\\\"","head":{"revision":1}}')
                intended = {"version": 2, "hub": "head-fixture", "revision": 2, "snapshot": "b" * 64}
                private_write(root / "upload", json.dumps(intended).encode())
                for name in ("body", "headers"):
                    private_write(root / name, b"")
                credential = 'synthetic-key"\\:synthetic-secret\\"'
                base = [("url", "https://fixture.invalid/uah-head.json"),
                        ("user", credential), ("aws-sigv4", "aws:amz:auto:s3"),
                        ("output", str(root / "body")), ("dump-header", str(root / "headers")),
                        ("write-out", "%{http_code}"), ("max-time", "1")]
                def send(additional):
                    document = "\n".join(key + " = " + json.dumps(value) for key, value in base + additional)
                    return subprocess.run([str(executable), "-q", "--config", "-"],
                                          input=document.encode(), capture_output=True, timeout=3,
                                          env={"PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"})
                lost = send([("request", "PUT"), ("header", 'If-Match: "first"'),
                             ("data-binary", "@" + str(root / "upload"))])
                self.assertEqual((lost.returncode, lost.stdout, lost.stderr), (56, b"000", b""))
                fetched = send([("request", "GET")])
                self.assertEqual((fetched.returncode, fetched.stdout, fetched.stderr), (0, b"200", b""))
                self.assertEqual(json.loads((root / "body").read_text()), intended)
                calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
                self.assertEqual([call["method"] for call in calls], ["PUT", "GET"])
                self.assertEqual(one(calls[0]["options"], "user"), credential)
                self.assertEqual(calls[0]["headers"]["if-match"], ['"first"'])
                self.assertNotIn("AWS_SECRET_ACCESS_KEY", calls[0]["environment"])
                self.assertNotIn(credential, calls[0]["argv"])
                self.assertTrue(all(file["regular"] and file["mode"] == 0o600
                                    and file["parent_mode"] == 0o700 for file in calls[0]["files"]))

        def test_conditional_create_replace_and_stale_token(self):
            with tempfile.TemporaryDirectory(prefix="ownerless-head-fixture-check-") as directory:
                root = Path(directory)
                private_write(root / "fixture.json", b"{}")
                private_write(root / "remote.json", b'{"exists":false,"etag":"\\\"first\\\""}')
                def send(method, condition, revision):
                    options = [f'request = "{method}"', f'output = "{root / "body"}"',
                               f'dump-header = "{root / "headers"}"', 'write-out = "%{http_code}"']
                    if condition:
                        options.append("header = " + json.dumps(condition))
                    if method == "PUT":
                        options.append("data-binary = " + json.dumps(json.dumps({"revision": revision})))
                    with patch("sys.stdout", io.TextIOWrapper(io.BytesIO())), patch("sys.stderr", io.TextIOWrapper(io.BytesIO())):
                        self.assertEqual(request(root, ["-q", "--config", "-"], "\n".join(options)), 0)
                    return json.loads((root / "calls.jsonl").read_text().splitlines()[-1])["status"]
                self.assertEqual(send("GET", None, 0), 404)
                self.assertEqual(send("PUT", "If-None-Match: *", 1), 200)
                self.assertEqual(send("PUT", "If-None-Match: *", 1), 412)
                self.assertEqual(send("PUT", 'If-Match: "new.etag/+==:2"', 2), 200)
                self.assertEqual(send("PUT", 'If-Match: "stale"', 3), 412)
                self.assertEqual(send("PUT", None, 4), 400)
                self.assertEqual(json.loads((root / "remote.json").read_text())["head"], {"revision": 2})

        def test_object_keys_conditional_get_put_and_delete(self):
            with tempfile.TemporaryDirectory(prefix="ownerless-object-fixture-check-") as directory:
                root = Path(directory)
                private_write(root / "fixture.json", b"{}")
                private_write(root / "remote.json", b'{"exists":false,"etag":"\\"absent\\"","objects":{}}')
                payload = bytes(range(256))
                private_write(root / "upload", payload)
                def send(method, name, condition=None, upload="upload"):
                    options = [f'request = "{method}"', "url = " + json.dumps(PREFIX + name),
                               f'output = "{root / "body"}"', f'dump-header = "{root / "headers"}"',
                               'write-out = "%{http_code}"']
                    if condition:
                        options.append("header = " + json.dumps(condition))
                    if method == "PUT":
                        options.append("data-binary = " + json.dumps("@" + str(root / upload)))
                    with patch("sys.stdout", io.TextIOWrapper(io.BytesIO())), patch("sys.stderr", io.TextIOWrapper(io.BytesIO())):
                        self.assertEqual(request(root, ["-q", "--config", "-"], "\n".join(options)), 0)
                    return json.loads((root / "calls.jsonl").read_text().splitlines()[-1])
                self.assertEqual(send("GET", "uah-live/a")["status"], 404)
                created = send("PUT", "uah-live/a", "If-None-Match: *")
                self.assertEqual((created["status"], created["key"], created["body_hex"]), (200, "uah-live/a", payload.hex()))
                self.assertEqual(send("PUT", "uah-live/a", "If-None-Match: *")["status"], 412)
                self.assertEqual(send("GET", "uah-live/a", 'If-None-Match: "new.etag/+==:2"')["status"], 304)
                self.assertEqual((root / "body").read_bytes(), b"")
                self.assertEqual(send("GET", "uah-live/a", 'If-None-Match: "stale"')["status"], 200)
                self.assertEqual((root / "body").read_bytes(), payload)
                self.assertIn(b'ETag: "new.etag/+==:2"', (root / "headers").read_bytes())
                self.assertEqual(send("PUT", "uah-live/a", 'If-Match: "stale"')["status"], 412)
                self.assertEqual(send("PUT", "uah-live/a")["status"], 400)
                self.assertEqual(send("GET", HEAD_NAME)["status"], 404)  # The head keeps its own state.
                large = b"z" * (INLINE_LIMIT + 1)
                private_write(root / "large", large)
                recorded = send("PUT", "uah-live/large", "If-None-Match: *", "large")
                self.assertEqual((recorded["status"], recorded["body"], recorded["body_hex"]), (200, None, None))
                self.assertEqual((recorded["body_size"], recorded["body_sha256"]), (len(large), hashlib.sha256(large).hexdigest()))
                self.assertEqual(send("DELETE", "uah-live/a")["status"], 204)
                self.assertEqual(send("DELETE", "uah-live/a")["status"], 404)
                self.assertEqual(send("GET", "uah-live/a")["status"], 404)
                self.assertEqual(json.loads((root / "remote.json").read_text())["objects"],
                                 {"uah-live/large": {"etag": '"new.etag/+==:3"', "body_size": len(large),
                                                     "body_sha256": hashlib.sha256(large).hexdigest()}})
                with self.assertRaises(ValueError):
                    request(root, ["-q", "--config", "-"], 'url = "https://elsewhere.invalid/bucket/key"')

    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(FixtureChecks))
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sys.exit(self_test())
    signal.alarm(10)  # Even a malformed product request cannot leave a fixture behind.
    try:
        case_root = Path(__file__).resolve().parent.parent
        # Limit stdin too; malformed or unterminated curl config cannot exhaust the fixture.
        config = sys.stdin.buffer.read(128 * 1024 + 1)
        if len(config) > 128 * 1024:
            raise ValueError("oversized stdin config")
        sys.exit(request(case_root, sys.argv[1:], config.decode()))
    except (OSError, ValueError, KeyError) as error:
        # Never echo config, credentials, or an uncontrolled exception to product diagnostics.
        try:
            append_call(case_root, {"fixture_error": type(error).__name__})
        except OSError:
            pass
        sys.stderr.write("offline head curl fixture rejected request\n")
        sys.exit(97)
