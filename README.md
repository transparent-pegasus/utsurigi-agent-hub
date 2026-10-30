# utsurigi-agent-hub

Named hubs for encrypted data and shared knowledge across personal AI platforms.

Give your Bot the [Bot instructions](BOT.md) to get started.
Setup, operations, and recovery guides load only when needed:
[INIT](INIT.md), [OPS](OPS.md), [RECOVERY](RECOVERY.md).

## Features

- External data: encrypted storage and recovery, through R2 or the Bot.
- Named hubs: choose each Bot's hub during setup.
- Periodic review: prefer native Bot features when equivalent or better.
- Setup through dialogue.
- [YAML configuration](config.yaml).

## Optional features

Task management, [Memory RAG](optional/README.md), Caveman. Tasks and Caveman default on; RAG off.

## Credentials and recovery

Store cloud credentials and repository passwords in 1Password or another secrets
SaaS, the Bot's Vault service, or both. Keep recoverable credential references and
repository locations there so recovery survives host loss. Inject secrets securely;
never put values in chat, commands, logs, or YAML. See [setup](INIT.md).

Investigate each environment's file-sharing facilities. Give authorized Bots in
one hub+machine the same `paths.imports` root. Shared knowledge uses stable
`versions/<snapshotID>/` names under `paths.imports/<SHA-256(hub)>/`, reuses matching
snapshot IDs, and switches JSON `CURRENT` atomically. Keep current and previous
versions; long reads follow the [reader lock protocol](OPS.md#publish-and-consume).
Choose the location for the environment; no Bot-specific layout is required.

Restic deduplicates data. Small SQLite changes may upload the entire database;
per-record incremental transfer and fixed sync latency are not guaranteed.

## Requirements

Bundled cloud scripts: R2 only. For S3, Google Cloud Storage, Google Drive, etc.,
write your own or ask your Bot to build them through dialogue.

For trusted Bots under one owner. Requires Unix and Rust.
Validated on Linux only.
Optional memory also needs C/C++ build tools; on Debian, run `apt install g++`.
See [optional memory](optional/README.md) for runtime and migration requirements.
