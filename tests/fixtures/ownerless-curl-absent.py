#!/usr/bin/env python3
"""CLI budget fixture: fake only HEAD absence; no network and no write fallback."""
import json
import os
from pathlib import Path
import shlex
import sys

os.umask(0o077)
options = {}
arguments = iter(sys.argv[1:])
for argument in arguments:
    if argument in ("--config", "-K"):
        if next(arguments) != "-":
            sys.exit(97)
        for line in sys.stdin.read().splitlines():
            key, separator, value = line.partition("=")
            if not separator:
                parts = line.strip().split(None, 1)
                key, value = parts[0], parts[1] if len(parts) > 1 else ""
            value = value.strip()
            if value.startswith('"'):
                try:
                    value = json.loads(value)
                except ValueError:
                    value = shlex.split(value)[0]
            options[key.strip().lstrip("-")] = value
    elif argument in ("--output", "-o", "--dump-header", "-D", "--write-out", "-w", "--request", "-X", "--url"):
        key = {"-o": "output", "-D": "dump-header", "-w": "write-out", "-X": "request"}.get(argument, argument.lstrip("-"))
        options[key] = next(arguments)
    elif argument.startswith("https://"):
        options["url"] = argument

expected = "https://ownerless-acceptance-42.r2.cloudflarestorage.com/budget/shared/uah-head.json"
if options.get("url") != expected or options.get("request", "GET") != "GET":
    sys.stderr.write("budget fixture refused unexpected URL or write\n")
    sys.exit(97)
with Path(__file__).with_name("requests").open("a") as log:
    log.write("GET\n")
headers = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"
destination = options.get("dump-header")
if destination and destination != "-":
    Path(destination).write_bytes(headers.encode())
elif destination == "-":
    sys.stdout.write(headers)
destination = options.get("output")
if destination and destination != "-":
    Path(destination).write_bytes(b"")
rendered = options.get("write-out", "").replace("%{http_code}", "404").replace("%{response_code}", "404").replace("\\n", "\n")
sys.stdout.write(rendered)
