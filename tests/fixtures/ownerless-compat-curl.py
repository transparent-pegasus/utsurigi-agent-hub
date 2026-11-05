#!/usr/bin/python3
"""Offline GET/conditional-PUT adapter for the compatibility tests' fake R2.

Installed as <fixture>/bin/curl; fixture.json names one exact head URL and a
head_path inside the generated local restic repository. No network code exists.
The config parser follows the independent ownerless head fixture's curl grammar.
"""

import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import sys
import tempfile


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
        raise ValueError("ambient curl config must be disabled")
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
            key, value = short[:1], short[1:]
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
        raise ValueError("stdin config required")
    return options


def one(options, name, default=None):
    values = [value for key, value in options if key == name]
    if len(values) > 1:
        raise ValueError("duplicate singleton option")
    return values[0] if values else default


def upload_body(options):
    for key in ("upload-file", "data-binary", "data-raw", "data"):
        value = one(options, key)
        if value is not None:
            if key == "upload-file" or (key != "data-raw" and value.startswith("@")):
                path = value if key == "upload-file" else value[1:]
                if path == "-":
                    raise ValueError("stdin is the config stream")
                return Path(path).read_bytes()
            return value.encode()
    return b""


def request(root, arguments, document):
    options = parse_request(arguments, document)
    fixture = json.loads((root / "fixture.json").read_text())
    if one(options, "url") != fixture["url"]:
        raise ValueError("unexpected URL")
    head_path = Path(fixture["head_path"])
    default_method = "PUT" if one(options, "upload-file") is not None else "GET"
    if default_method == "GET" and any(key in ("data", "data-binary", "data-raw") for key, _ in options):
        default_method = "POST"
    if one(options, "head") is not None:
        default_method = "HEAD"
    method = one(options, "request", default_method).upper()
    headers = {}
    for key, value in options:
        if key == "header":
            name, separator, content = value.partition(":")
            if not separator:
                raise ValueError("invalid header")
            headers.setdefault(name.lower(), []).append(content.strip())
    with (root / "object.lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        previous = head_path.read_bytes() if head_path.exists() else None
        etag = '"' + hashlib.sha256(previous).hexdigest() + '"' if previous is not None else None
        if method == "GET":
            status, response = (200, previous) if previous is not None else (404, b"")
        elif method == "PUT":
            create = headers.get("if-none-match") == ["*"] and "if-match" not in headers
            replace = headers.get("if-match") == [etag] and "if-none-match" not in headers
            if "if-match" in headers and "if-none-match" in headers:
                status = 400
            elif not create and "if-match" not in headers:
                status = 400  # Never turn an unconditional PUT into a success.
            elif (create and previous is not None) or (not create and not (replace and previous is not None)):
                status = 412
            else:
                body = upload_body(options)
                head = json.loads(body)
                if set(head) != {"version", "hub", "revision", "snapshot"} or head["version"] != 2:
                    raise ValueError("invalid head object")
                if not isinstance(head["revision"], int) or isinstance(head["revision"], bool) or head["revision"] <= 0:
                    raise ValueError("invalid revision")
                if not isinstance(head["hub"], str) or not re.fullmatch("[0-9a-f]{64}", head["snapshot"]):
                    raise ValueError("invalid identity")
                descriptor, temporary = tempfile.mkstemp(dir=head_path.parent)
                with os.fdopen(descriptor, "wb") as stream:
                    stream.write(body)
                os.replace(temporary, head_path)
                status = 200
            response, etag = b"", None
        else:
            raise ValueError("only GET and conditional PUT are supported")
        with (root / "requests.jsonl").open("a") as log:
            # Log only the nonsecret object-store contract, never signing data.
            conditions = {name: values for name, values in headers.items()
                          if name in ("if-match", "if-none-match")}
            log.write(json.dumps({"method": method, "status": status, "conditions": conditions}) + "\n")
    response_headers = f"HTTP/1.1 {status} Fixture\r\nContent-Length: {len(response)}\r\n"
    if etag is not None:
        response_headers += f"ETag: {etag}\r\n"
    response_headers = (response_headers + "\r\n").encode()
    for key, body in (("output", response), ("dump-header", response_headers)):
        path = one(options, key)
        if path and path != "-":
            Path(path).write_bytes(body)
        elif key == "output" or one(options, "include") is not None:
            sys.stdout.buffer.write(body)
    rendered = one(options, "write-out", "")
    rendered = rendered.replace("%{http_code}", str(status)).replace("%{response_code}", str(status))
    sys.stdout.write(rendered.replace("%{size_download}", str(len(response))))
    return 22 if status >= 400 and any(key in ("fail", "fail-with-body") for key, _ in options) else 0


if __name__ == "__main__":
    os.umask(0o077)
    signal.alarm(10)
    try:
        document = sys.stdin.buffer.read(128 * 1024 + 1)
        if len(document) > 128 * 1024:
            raise ValueError("oversized config")
        sys.exit(request(Path(__file__).resolve().parent.parent, sys.argv[1:], document.decode()))
    except (OSError, ValueError, KeyError, TypeError):
        sys.stderr.write("offline compatibility head fixture rejected request\n")
        sys.exit(97)
