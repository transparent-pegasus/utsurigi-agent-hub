---
description: Setup, hub membership, storage, and optional capabilities.
when: No Bot YAML, status.ready=false, or setup/add/join Bot or hub requested.
---
# Initialize

Read [BOT.md](BOT.md) first. Load [practices](knowledge/practices.md) when choosing
capabilities or changing policy. Add this framework through the host's supported
knowledge facility; never replace its instructions.

## Runtime

Use trusted Bots under one owner. The Rust CLI uses restic for encryption,
snapshots, and recovery. The host handles dialogue, secret delivery, scheduling,
and shared knowledge. Unattended sync needs scheduling and secure secret injection.

The host needs Unix, Rust, direct network access, writable storage, and the system
trust store. Custom CA roots must use that store; acquisition and storage children
do not inherit proxy or custom-CA environment settings. Restic acquisition needs
HTTPS curl. Setup reuses the configured version or downloads and checksum-verifies
the official release. Runtime validation covers Linux.

```sh
cargo build --locked --release
./target/release/utsurigi-agent-hub validate --template
./target/release/utsurigi-agent-hub --help
```

Older storage formats have no automatic migration. Restic metadata
records the OS username, hostname, and absolute staging paths. Restored data is
readable by the host.

## Dialogue and configuration

Infer reliable language, timezone, host capabilities, and existing settings.
Ask only for missing Bot identity, hub membership, storage access, secret access,
data scope, or adoption choices that would change existing behavior; combine related
gaps.
Check existing same-owner authorized configurations and durable secret/repository
references. With the user, select an existing authorized hub or create and name one
before initializing storage. The sole initial candidate is `default`, which may be
kept; never infer a hub from the platform. Names must be unique within the owner's
managed hubs and use 1–128 ASCII letters, digits, `.`, `_`, or `-`, excluding `.`
and `..`. Quote numeric-looking names in YAML. A stored hub name is stable;
editing one YAML is not a supported rename.
At hub initialization, ask whether to enable Caveman (default: `true`). Record the
answer in hub knowledge and each Bot's `skills.caveman.enabled`; joining Bots reuse it.
Generate runtime IDs and opaque repository paths. Keep optional defaults unless
requested. Preserve existing memory sources and direct or delegated retrieval
choices. Record applicable sources in `memory.existing_source` and delegated
retrieval Bots in `memory.retrieval_bot_id`, each as one prose string, not a list
or object. Preserving an existing system must not silently enable framework RAG
(`memory.enabled`). Disabled RAG needs no model questions. Never request secret
values in chat.

Let the initializing Bot investigate machine identity, file sharing, and existing
resources. Reuse the first compatible installation within the same ownership and
access scope. Read an available machine identifier; never generate one. Record
`machine.id` as a quoted string and `machine.root` in each Bot's private YAML;
leave unavailable IDs null. Keep machine metadata out of shared knowledge and
synchronized content.

Keep one managed copy of shared memory and common knowledge per hub outside Bot
workspaces. Reuse existing native resources in place within that hub's authorized data scope
and record their paths in each Bot's YAML.
Use separate roots when machines or services cannot share files. Recheck identity
and access after moving environments; never overwrite an existing installation.
Coordinate initialization and shared-resource updates. Preserve private memory
boundaries; keep Bot data, sync state, and Bot locks separate. Assign shared
memory exports to one Bot; synchronize consistent snapshots, never a live database.

For each hub+machine, configure the same absolute `paths.imports` root in every
Bot YAML. Investigate the environment's sharing facilities and permissions first;
choose an authorized common folder or native knowledge facility. No Bot-specific
layout is required. Use distinct roots for different hubs or machines.
The shared cache uses stable `versions/<snapshotID>/` directories and an atomically
replaced `CURRENT` pointer; see [operations](OPS.md) for reuse and retention.

Read [config.yaml](config.yaml), then write each Bot's private YAML outside
checkouts or under ignored `.utsurigi-agent-hub/<hub>/<bot-id>/config.yaml`.
Pass its path with `--config` to every command, including scheduled runs. Paths
resolve beside that YAML. Keep `machine.root` outside checkouts and Bot workspaces;
it may contain several hubs. Keep owned, shared authoring, shared imports, and
per-Bot state roots separate beneath it or in existing authorized locations;
checkout-local roots must be Git-ignored. The host sets resource paths and verifies
the intended filesystem is mounted and accessible. The CLI does not derive paths
from `machine.root`; it creates missing state and imports directories. Use real
paths without symlink ancestors. Classify data into synchronized content, external
references, and local regenerable files. Translate the synchronized scope into `data.include`.
Unclassified data stays excluded. Preserve unique memories, unfinished work,
and unpushed Git changes through appropriate exports; a Git URL cannot recover
them.

An explicitly requested second hub membership needs separate YAML, data, shared
authoring/imports, state, local repositories, and scheduling. Never copy or pull
across hubs automatically. Reuse the machine ID, immutable model assets, common
code, and a verified shared `encryption.executable` when available. Memory databases, native
stores, and runtime caches/sockets must not silently span hubs. A shared model
`UTSURIGI_MEMORY_HOME` needs an explicit per-hub `UTSURIGI_MEMORY_PATH` (or a separate
HOME) and an isolated cache/socket. Private Bot access still applies within each hub.
Host-provisioned access, repositories, and data roots enforce hub scope; name
checks catch mistakes and do not replace permissions.

For detailed setup, derive and verify the YAML's GitHub blob URL from the actual
repository and revision. Record it in `onboarding.config_blob_url`, offer it for
editing, and request the edited YAML upload. Until the file is published, offer
the bundled YAML instead.

Keep repository passwords and cloud credentials in 1Password or another secrets
SaaS, the Bot's Vault service, or both. Recovery credentials and repository locations
must survive loss of the execution host. Verify the active account before access.
Deliver secrets through secure process injection, never model-visible output or
screens. Without injection, keep sync assisted.
Never create plaintext bootstrap-token files.
`secrets.confirm_each_access: false` skips extra framework prompts only; vendor
access checks and existing authorization still apply.

The host supplies each named `password_env` value to the process. The CLI maps it
to child `RESTIC_PASSWORD`; R2 also uses `AWS_ACCESS_KEY_ID`,
`AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`. Keep values out of
commands, logs, chat, and YAML. Store the hub name, local and remote repository
locations, and recoverable password references in the same secure store so
recovery survives loss of local configuration.

Provision object-scoped R2 access. With `budget.free_tier_only`, writes require
a JSON attestation at `budget.attestation_file`, owned by the process user with
owner-only permissions (0600). It contains `expires_at` (RFC3339),
`repositories` (exact URLs), and `max_source_bytes` (integer). Issue it only
after checking account usage, quota, billing, and encryption overhead. Unknown
costs keep writes held; the CLI does not audit billing. Local repositories need no
CLI cloud attestation.

If the user has no R2 storage, use an authorized Google Drive, S3, or other store
through the Bot. Set `storage.provider: local` with distinct absolute local repository
paths. Record remote destinations and transfer/recovery policy in
`storage.bot_transfer` as one prose string; the CLI does not execute it. Check
transfer capabilities, quotas, and costs; honor `budget.free_tier_only`.

Assign one transfer coordinator per repository. Reconcile the latest complete
remote generation before local operations and coordinate all writers. Transfer
complete encrypted repositories only while quiescent, preserving registry history
and private access boundaries. Never upload plaintext sources or secrets, copy
live repositories, or merge divergent copies. Publish a new generation only after
verification; retain the prior verified generation until its successor is recoverable.

In this fallback, `sync` success and timestamps confirm only local snapshots.
Track remote completion and pending transfers separately from `status.next_due`.
If transfer cannot be verified, report it pending and keep originals. For recovery,
fetch a verified generation into a fresh local directory. In a private recovery
YAML, retain the original `hub` and point the selected `repositories.shared.url`
or `repositories.bot.url` to that directory. Check integrity, list snapshots, and
restore using that same repository path and YAML. Keep the prior verified generation.
Before resuming normal operations, reconcile repository locations and registry
authority with the owner.

Create the owner's `paths.shared/registry.yaml` using this schema:

```yaml
version: 1
hub: default
owner: {bot_id: owner, runtime_id: generated-runtime}
bots:
  - id: owner
    runtime_id: generated-runtime
    repository: "s3:https://ACCOUNT.r2.cloudflarestorage.com/BUCKET/OPAQUE-BOT-PATH"
    password_ref: "secret-provider reference"
    password_env: BOT_PASSWORD
    shared: true
```

The URL above is a schema placeholder; replace ACCOUNT, BUCKET, and OPAQUE-BOT-PATH.
Runtime validation rejects placeholder URLs. Use actual configured values, including
the selected hub. Bot IDs are unique within a hub's registry and may be reused in
separate hubs. Each Bot membership gets its own repository; that hub's shared
knowledge has another.
The registry owner publishes admissions and policy changes; joining
Bots consume that registry. Prepare the allowlisted files, validate the local
configuration, invoke `setup`, then verify one `sync` and recovery before
scheduling.

## Optional capabilities

Choose memory and task systems under the shared [review policy](knowledge/practices.md).
Honor the user's selection. Keep unfinished tasks. Do not impose a task format.
Disabling framework tasks or RAG (`memory.enabled`) never disables native or
existing systems.

Only when framework RAG is enabled and needed, research current public text
retrieval models for the user's language, retrieval quality, memory/CPU budget,
license, and Rust/backend compatibility. Record the choice in
`memory.embedding_model` as one prose string including its source URL, not a list
or object; do not reselect on startup. Bundled [utsurigi-memory](optional/README.md) supports
its pinned E5 model or explicit hash mode;
a model name alone does not add backend support. No multimodal pipeline.

Only when Caveman is enabled, clone its configured source, pin a verified
revision in `skills.caveman.ref`, and share its skill reference. Install only the
prose skill; exclude proxy, gateway, and model-routing features.
