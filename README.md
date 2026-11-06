# utsurigi-agent-hub

Keep Bot data in one external store. Then:

- Send tasks to Bots on other services. Share knowledge with them.
- Use the same Bot on other services.

Give your Bot the [Bot instructions](BOT.md) to get started.
Setup, operations, and recovery guides load only when needed:
[INIT](INIT.md), [OPS](OPS.md), [RECOVERY](RECOVERY.md).

## First prompt for your Bot

Copy this prompt into the Bot you want to set up:

```text
Clone https://github.com/transparent-pegasus/utsurigi-agent-hub .
Read BOT.md first, then INIT.md; follow their references to other Markdown guides.
```

## Features

- External data: encrypted storage and recovery, through R2 or the Bot.
- Named hubs: choose each Bot's hub during setup.
- Live sync: tasks and rules reach the Bot's other machines and peers within
  minutes, one read per check. See [OPS](OPS.md#live-sync).
- Periodic review: prefer native Bot features when equivalent or better.
- Setup through dialogue.
- [YAML configuration](config.yaml).

## Optional features

Task management, [Memory RAG](optional/README.md), Caveman. Template defaults are
tasks and Caveman on, RAG off. During setup, override an optional feature's default
to `false` when the Bot already covers it and the optional feature supplies no
capability missing from the Bot. An explicit user decision of `true` wins.

## Credentials and recovery

Choose secret delivery in this order: a secrets SaaS connector (such as 1Password),
the Bot service's Vault, then other secure host facilities. If none can be used,
an attached `.env` is the final fallback. See the researched
[delivery options](SECRETS.md) and [setup procedure](INIT.md#secrets-and-transfer-before-externalization).
Keep credentials and repository locations recoverable after host loss. Never put
values in chat text, command arguments, logs, or YAML. The CLI reads an optional
owner-only `0600` `.env` beside the selected config; existing process variables win.
[.env.example](.env.example) lists all supported input variables.

Setup passes only after `restic check`, a restore compared byte for byte with the
captured backup source, and verified remote retrieval when the Bot handles transfer.

Give Bots in one hub+machine the same `paths.imports` root. See
[OPS](OPS.md#publish-and-consume) for the cache layout, reader locks, and sync behavior.

## Requirements

Bundled cloud scripts: R2 only. For S3, Google Cloud Storage, Google Drive, etc.,
write your own or ask your Bot to build them through dialogue.

For trusted Bots under one owner. Requires Unix and Rust.
Validated on Linux only.
Optional memory also needs C/C++ build tools; on Debian, run `apt install g++`.
See [optional memory](optional/README.md) for runtime and migration requirements.
