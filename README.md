# utsurigi-agent-hub

Named hubs for encrypted data and shared knowledge across personal AI platforms.

Give your Bot the [Bot instructions](BOT.md) to get started.

## Features

- External data: encrypted storage and recovery, through R2 or the Bot.
- Named hubs: choose each Bot's hub during setup.
- Periodic review: prefer native Bot features when equivalent or better.
- Setup through dialogue.
- [YAML configuration](config.yaml).

## Optional features

Task management, [Memory RAG](optional/README.md), Caveman. Tasks and Caveman default on; RAG off.

## Policies

Follow native instructions. Prefer native features; optionally adopt compatible
improvements or missing capabilities.
Initialize with shared [principles](knowledge/principles.md) and [practices](knowledge/practices.md):

- Use Rust by default.
- Prefer capable connectors; otherwise use authorized computer automation.
- Prefer native Bot scheduling for periodic tasks.
- Preserve data externalization; review everything else on schedule or capability changes and propose revisions.
- Prefer policies when procedures are incomplete or unsuitable. Let agents choose the steps.
- Bound retries for transient network failures. Back off reads; reconcile uncertain writes before retrying.

Bundled cloud scripts: R2 only. For S3, Google Cloud Storage, Google Drive, etc.,
write your own or ask your Bot to build them through dialogue.

For trusted Bots under one owner. Requires Unix and Rust.
Validated on Linux only.
