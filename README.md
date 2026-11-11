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

- External data: encrypted storage and recovery, through Google Drive via the
  Bot's connector (default), R2, or other stores via the Bot.
- Named hubs: choose each Bot's hub during setup.
- Live sync: tasks and rules reach the Bot's other machines and peers within
  minutes on R2, one read per check; with Bot transfer, after each transfer.
  See [OPS](OPS.md#live-sync).
- Periodic review: prefer native Bot features when equivalent or better.
- Setup through dialogue.
- Required [shared tasks](TASKS.md): dependencies, user waits, goals, logs, and archived history across Bots.
- [YAML configuration](config.yaml).

## Optional features

[Memory RAG](optional/README.md), Caveman. Template defaults are
Caveman on, RAG off. During setup, override an optional feature's default
to `false` when the Bot already covers it and the optional feature supplies no
capability missing from the Bot. An explicit user decision of `true` wins.

Task management is mandatory for new and existing Bots. Migrate GrokBot/Muse
records through [TASKS](TASKS.md); native schedules remain with their Bot.
Legacy `tasks.enabled` values are accepted for compatibility and do not disable tasks.

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

Bundled cloud scripts: R2 only. R2 uses fewer model tokens than other storage:
the scripts run it without Bot transfer work. For S3, Google Cloud Storage, etc.,
write your own or ask your Bot to build them through dialogue.

For trusted Bots under one owner. Requires Unix and Rust.
Validated on Linux only.
Optional memory also needs C/C++ build tools; on Debian, run `apt install g++`.
See [optional memory](optional/README.md) for runtime and migration requirements.

## Upgrading

Before updating an installed checkout, follow [upgrading](upgrading/README.md).
It lists the per-version migration guides.
