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

Always begin: "Is this a new Bot or a continuation of an existing Bot?" Ask even
when inferable. For continuation, reuse that Bot's ID. Group related questions
into 1–2 exchanges, including this opening question.

For both new and continuing Bots, reuse approved decisions for the same owner,
hub, and purpose; do not ask for those decisions again.

Template values are unselected candidates, not consent. Ask about unsettled
decisions even when YAML is populated. `Config::missing` and `ready` do not check
ASK agreement; missing/null checks cannot replace this dialogue.

**ASK** about unsettled choices:

- `hub`: "Which authorized hub should this Bot join, or should we create one
  (initial candidate: `default`)?"

- `storage.provider`: "Which backup destination and account should we use: R2
  (account and bucket), an authorized Drive, S3, or another store?"

- `secrets.provider`: "Which service, account, and place should hold backup
  credentials and recovery passwords: 1Password, the Bot's Vault, or another
  store? How will we recover if this host is lost?" Reuse an existing approved
  store for the same owner, hub, and purpose. Never request secret values in chat;
  use secure connection facilities.

- `data.include`: Investigate concrete candidates first. "May we back up A and B,
  keep C as an external reference, and keep regenerable D local? What should we
  add or exclude? What common knowledge may enter the shared authoring scope
  (`paths.shared`)?" `data.include`/`data.exclude` cover Bot-owned data; they do
  not limit the shared authoring scope.

- `sharing.bot_data`: "May authorized Bots in this hub retrieve this Bot's
  backed-up data (default: `true`)?" If kept private, backup and access to the
  hub's common knowledge continue.

- `tasks.enabled`, only for new adoption: "Should this framework also record
  and track tasks (default: `true`)?" Preserve existing methods and unfinished
  tasks; reuse approved operation without asking again.

- `skills.caveman.enabled`, only at hub initialization: "Should this hub use
  Caveman to shorten responses (default: `true`)? Existing instructions take
  priority; proxy, gateway, and model-routing features are excluded." Record
  the answer in hub knowledge and each Bot's YAML; joining Bots reuse it.

- `storage.bot_transfer`, only for `local` storage or when an external destination
  or transfer policy needs deciding: "Which destination and automatic or assisted
  transfer and recovery policy should we use?" Local snapshots do not prove
  external backup completion.

- `budget.free_tier_only`, only after the free tier is proven insufficient:
  "Should we reduce scope to fit the free tier, or authorize paid use?" Never
  silently set it to `false`; hold writes while costs or authorization remain
  unresolved.

**INFER** technical values from facts and approved choices; investigate and record
them with no individual confirmation. Ask only for missing required facts or
unresolved choices that change permissions.

- Infer reliable `language`, `timezone`, `bot.platform`, and host capabilities.
  Derive `bot.id` from the opening answer; reuse the existing Bot ID for
  continuation. Infer `bot.role` from existing instructions; ask if the role is
  unavailable. Reuse `bot.runtime_id` for the same runtime; generate once for a
  new runtime and stop the prior runtime before handover.
- Investigate `machine.id`, `machine.root`, and `paths.*` as described below.
- Derive `storage.credentials_ref` and `repositories.*` URL, `password_ref`, and
  `password_env` values only after the store and account are approved. Generate
  opaque repository paths when new ones are needed.
- Derive `data.exclude`, `data.sync`, `data.reference`, `data.local`, and
  `data.sqlite_snapshots` from approved scope and verified consistent exports.
- Resolve `encryption.executable`, `budget.attestation_file`, `skills.caveman.ref`
  when enabled, and `onboarding.config_blob_url` from verified resources.
- Record `memory.existing_source` and `memory.retrieval_bot_id`; infer
  `memory.embedding_model` only when framework RAG is adopted.

**YAML-ONLY**: preserve current values or template defaults; do not reset prior
approved settings. No setup questions or automatic tuning for schema `version`,
structure keys, `storage.class`, `encryption.version`, `sync.*` (interval, start
time, retention, retry, transport, timeout, bandwidth, concurrency),
`budget.max_bots`, `budget.storage_target_gb`, `secrets.confirm_each_access`,
`memory.enabled` (default: `false`), `skills.caveman.source`, or `review.reviewer_bot`.

Check existing same-owner authorized configurations and durable secret/repository
references. Reuse the approved hub; if unsettled, select an existing authorized hub
or create and name one with the user before initializing storage.
The sole initial candidate is `default`, which may be
kept; never infer a hub from the platform. Names must be unique within the owner's
managed hubs and use 1–128 ASCII letters, digits, `.`, `_`, or `-`, excluding `.`
and `..`. Quote numeric-looking names in YAML. A stored hub name is stable;
editing one YAML is not a supported rename.
Preserve existing memory sources and direct or delegated retrieval
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
The managed cache root is `paths.imports/<SHA-256(hub)>/`. It holds stable
`versions/<snapshotID>/` directories using full restic IDs, JSON `CURRENT`
(`hub`, `current`, `previous`), and `operation.lock`.
See [operations](OPS.md) for atomic switching, reader locks, reuse, and retention.

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

Only if the user requests detailed setup, offer YAML editing and an edited upload.
Derive and verify the YAML's GitHub blob URL from the actual repository and revision;
record it in `onboarding.config_blob_url`. Until published, offer the bundled YAML.
Dialogue-only users need no YAML editing or upload.

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
Bots consume that registry.

**Confirm once.** At the end of the dialogue, show ASK answers and INFER results
in one concise list and get one confirmation. Include the current
`secrets.confirm_each_access` value; vendor approval still applies. Show
"Framework RAG is disabled; can enable on request" for the normal default;
honor any existing approved RAG state. Add one line: "Details are configurable
in YAML."

Prepare the allowlisted files, validate the local configuration, invoke `setup`,
then verify one `sync` and recovery before scheduling.

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
