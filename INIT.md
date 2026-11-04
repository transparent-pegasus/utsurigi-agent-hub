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
Before relocating/exporting Bot data or initializing its backup repositories,
complete [secret and transfer preflight](#secrets-and-transfer-before-externalization).
The inventory and private configuration steps below prepare that preflight.

The host needs Unix, Rust, network access, writable storage, and trusted CA roots.
Acquisition and storage children receive the proxy and CA variables listed in
[.env.example](.env.example); other ambient settings are filtered. Test the actual
network path, including any proxy and custom CA. Restic acquisition needs HTTPS
curl. Setup reuses the configured version or downloads and checksum-verifies the
official release. Runtime validation covers Linux.

```sh
cargo build --locked --release
./target/release/utsurigi-agent-hub validate --template
./target/release/utsurigi-agent-hub --help
```

Restic metadata records the OS username, hostname, and absolute staging paths. Restored data is
readable by the host.

## Dialogue and configuration

Always begin: "Should we initialize or update this Bot using an existing Bot's
individual data in the Hub (continuation), or proceed without that data (new Bot)?"
Ask even when inferable. This data-use decision is the boundary: the age of the
Bot, a previous framework installation, and existing local files do not decide it.
For continuation, identify the authorized Hub data and reuse that Bot's ID; restore
and inspect it before adoption. Reuse its repository and password reference as well;
relocation is a separate recovery operation. Continuation hands over or updates
that identity: stop its previous runtime before activating the replacement.
An independently running clone is a separate operation whose identity and data
scope must be agreed explicitly. For new Bots, allocate a new ID,
repository destination, and password reference in the chosen Hub without replacing
local data or reusing retired identifiers. Generate its password only after the
empty-destination check below. Group related questions into 1–2 exchanges.

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

- `secrets.provider`: Investigate [delivery options](SECRETS.md) first, preferring
  secrets SaaS connectors, then the Bot service's Vault, then other secure host
  facilities; an attached `.env` is the last resort. "Which available service,
  account, and delivery method should hold backup credentials and recovery
  passwords? How will we recover after host loss?" Reuse approved choices for
  the same owner, hub, and purpose. Request secure injection, never chat values.

- `data.include`: Investigate concrete candidates first. "May we back up A and B,
  keep C as an external reference, and keep regenerable D local? What should we
  add or exclude? What common knowledge may enter the shared authoring scope
  (`paths.shared`)?" `data.include`/`data.exclude` cover Bot-owned data; they do
  not limit the shared authoring scope.

- `sharing.bot_data`: "May authorized Bots in this hub retrieve this Bot's
  backed-up data (default: `true`)?" If kept private, backup and access to the
  hub's common knowledge continue.

- `tasks.enabled`, only for new adoption: "Should this framework also record
  and track tasks (default: the value derived below)?" Preserve existing methods
  and unfinished tasks; reuse approved operation without asking again.

- `skills.caveman.enabled`, only at hub initialization: "Should this hub use
  Caveman to shorten responses (default: the value derived below)? Existing
  instructions take priority; proxy, gateway, and model-routing features are excluded." Record
  the answer in shared `decisions.yaml` and each Bot's YAML; joining Bots reuse it.

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

Before presenting optional defaults, compare the Bot's existing capabilities with
each optional feature. If the Bot already provides it and the optional feature
has no capability missing from the Bot, replace its template default with
`false`. If it fills a gap, keep the template candidate and discuss adoption
under the existing policy. Never disable the native capability. A user decision
of `true` from this or an earlier dialogue overrides the derived default; preserve
an approved `false` as well. This rule covers tasks, RAG, and Caveman, not backup
or Bot-data sharing. Hub-wide decisions already approved are reused by joining Bots.
For Caveman, distinguish an explicit hub-wide user decision from a template value:
only the former overrides the Bot-specific default derived from its capabilities.
Keep Bot-specific values and their basis in the Bot's YAML. Record hub-wide Caveman
values in shared `decisions.yaml` using the [typed schema](src/decisions.rs):
`version: 1`, the configured `hub`, and `repository_sha256`, plus a `caveman` mapping.
The digest is the lowercase SHA-256 of the configured shared URL's UTF-8 bytes,
verbatim, without an added newline. The mapping requires boolean `enabled`,
`basis` (`user_decision` or `derived_default`), nonempty `source` and `recorded_by`,
and RFC3339 `recorded_at`. Set `original_decision_at` to its known RFC3339 time or
`null`; do not invent it. `user_decision` requires explicit hub-wide confirmation;
`derived_default` records a default without claiming that confirmation. Approving
one Bot's generated YAML does not turn its default into a hub-wide user decision.

**YAML-ONLY**: preserve approved values or the defaults derived above; do not reset
prior approved settings. No setup questions or automatic tuning for schema `version`,
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
Choose a config directory outside owned/shared/import/export roots as well, so
it can hold the private fallback `.env` if needed; these constraints all apply.
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

## Secrets and transfer before externalization

Follow this order. Do not export real Bot data, initialize its repositories, or
start backup/remote transfer until secret injection and the transfer method have
both been selected **and successfully exercised**. A connector listing, a saved
setting, `validate`, or `status.ready` alone is not proof. Research the current
host's capabilities using [SECRETS.md](SECRETS.md), including unattended execution
and recovery after host loss. Confirm the unsettled ASK choices and INFER results
as described below before these tests use the selected account or destination.

1. Select secret delivery and the authorized storage/transfer route. Record account,
   scope, repository locations, credential references, environment variable names,
   and recovery procedure, never values. Verify the active account. Prefer SaaS
   connectors, then the Bot service's Vault, then other secure facilities; use
   the attached `.env` procedure below only if none can be selected.
2. Inspect each exact repository destination using storage-level listing or the
   mounted filesystem. Only after proving that destination empty, generate its
   restic password inside the chosen secret-sharing service as a **new item** and
   keep its recoverable reference. Never overwrite an existing password item/ref.
   An existing shared repository and a new Bot repository are
   separate decisions. If any data exists, request injection of its **existing**
   password; never replace it or initialize over it. A missing mount, denied list,
   network failure, restic exit 10/12, or unknown state does not prove emptiness.
   Stop for the original credentials or storage recovery. When only `.env` is
   possible, the owner generates and retains new passwords in an independently
   recoverable private store and delivers the file; the Bot never prints them.
3. **Prepare the pinned restic binary:** before any raw restic or CLI restore,
   provision the exact `encryption.version` (template: `0.19.1`). Reuse a trusted,
   verified binary, or obtain the matching platform archive from the official
   release for that version ([0.19.1](https://github.com/restic/restic/releases/tag/v0.19.1))
   and verify it against that release's SHA-256 checksums before installation.
   Keep it at a durable absolute path outside probe cleanup and
   set `encryption.executable` to that path in the real and probe YAML. Confirm
   `restic version` matches. Alternatively, do the isolated probe below first:
   its `setup` can acquire and verify restic; retain the verified executable at
   the durable path before removing probe state. `restore` does not acquire
   restic. Never use the real Bot's `setup` just to obtain the executable.
4. **Injection test:** launch through the exact secret-delivery mechanism that
   manual/scheduled operations will use. Prove credentials reach the CLI and the
   intended restic child without exposing values. For existing storage, use raw
   restic `snapshots --json` with secure injection, then CLI `restore` of a known
   complete snapshot into a fresh private directory. For empty storage, use only
   the isolated probe described below. Do not use the real Bot's `setup`, `sync`,
   `registry publish`, or `prune` as an injection test. Check scheduled
   execution separately before enabling unattended sync. A tool returning a secret
   into model-visible text does not qualify as injection.
5. **Transfer test:** use non-sensitive fixture bytes in that isolated probe,
   exercise upload and fresh download over the selected network/connector route,
   run `restic check`, restore, and compare every fixture byte. For Bot-mediated
   transfer, test a complete quiescent encrypted repository, not just a local
   snapshot or an upload receipt. Observe quota and cost authorization first.
   Retain failure evidence without secrets and keep externalization held until
   both tests pass; only then call the methods confirmed. Clean up identified
   disposable probe resources without touching existing data.

For the probe, prepare a separate private YAML with a temporary Hub/Bot identity,
two distinct empty repository destinations, isolated owned/shared/import/state
roots, a small non-sensitive fixture, and its initial registry. Keep the real Bot's
YAML unchanged. Verify both probe destinations empty, then create disposable
password items through the selected delivery route. Use the probe YAML for CLI
`setup`/`sync` and `restore`; these operations may externalize only the fixture.
R2 access must cover the exact probe prefixes, and its cost attestation must name
those exact URLs. If that access or budget is unavailable, leave preflight pending.
For Bot transfer, fetch the completed encrypted probe generation into a fresh
directory and use a separate recovery YAML for the comparison. Once both tests
pass, delete only the identified probe prefixes via authorized storage operations
and remove the disposable credential items and private probe files; restic has
no delete-repository command. Never broaden access or delete an existing prefix
to make a probe work.

The host supplies each YAML `password_env` value to the CLI; it maps only the
selected repository's value to child `RESTIC_PASSWORD`. R2 additionally needs
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.
Provider bootstrap credentials belong to the secure launcher, not YAML or a
model-visible tool result. Never create extra plaintext bootstrap-token files.
`secrets.confirm_each_access: false` removes only extra framework prompts; vendor
access checks remain. Keep passwords, credential references, hub name, repository
locations, and the host-loss recovery route recoverable outside the execution host.

### Attached .env fallback

Only when higher-priority delivery cannot be used, investigate a private `.env`
attachment through the host's authorized file-transfer facility. Before requesting
any real secrets, test a dummy `.env` containing only a public, non-secret marker.
Verify that the facility delivers the file to the execution environment without
automatically inserting its contents into model context, conversation records, or
tool/log output. Not opening the file with a Bot tool is not proof of this.
If contents are automatically exposed, or delivery behavior cannot be verified,
this route is unavailable: keep externalization held and choose another facility.

Only after that proof may the owner supply the real file, never pasted chat text.
This is an explicit exception for one local plaintext credential file; do not copy
its contents into prompts, tool output, shell history, logs, or YAML. If a real file
is accidentally exposed, stop and treat its credentials as disclosed. Coordinate
revocation/rotation with the owner and follow [recovery](RECOVERY.md), retaining
working recovery credentials until their replacements are verified. A password
change alone cannot revoke repository keys already disclosed. Deleting the
attachment does not undo disclosure.

- Delivery can have two stages: first supply only storage/connection credentials
  needed to inspect the exact destinations. Do not pre-generate repository
  passwords. After proving emptiness, the owner adds newly generated passwords;
  for existing or uncertain storage, supply the original passwords instead.
  The owner provides the completed replacement file, and the host verifies its
  scope, owner, and 0600 mode before explicitly replacing the private `.env`.
  Repeat upload-copy cleanup after every replacement; retain the previous private
  file until the replacement is verified, then remove that superseded local copy.
- Place it beside the private Bot config, outside source checkouts or under the
  ignored `.utsurigi-agent-hub/<hub>/<bot-id>/` directory. Keep that directory
  private (`0700`), outside owned/shared/import/export roots. Use one config
  directory per credential scope so unrelated Bots do not share one `.env`.
- The host executor handles the attachment without returning its contents to the
  model or opening it through a model-visible file reader. Before copying bytes,
  create a new private destination owned by the executing user with mode **0600**;
  a direct move is allowed only after verifying the source has that owner/mode.
  Verify the private destination, then remove remaining upload/staging copies.
  Never overwrite an existing `.env` silently. If the platform retains attachments,
  delete the remote attachment using its supported controls too, or ask the owner
  to delete it when only they can do so. Report unavailable deletion and arrange
  rotation/revocation instead of claiming every copy is gone. Retain the owner's
  separate recovery copy.
- The CLI automatically reads only `.env` next to the canonical `--config` file,
  before any operation. Missing files are allowed. Existing process variables,
  including empty ones, take priority. If either spelling of a proxy case pair
  exists in that original process environment, both `.env` spellings are ignored.
  Invalid syntax, symlinks, non-regular files, wrong ownership, or a mode other
  than 0600 stop startup without showing values.
  Do not shell-source the file. Its literal syntax and complete input-variable
  list are in [.env.example](.env.example). Provider references such as `op://...`
  are resolved by their launcher, not by the dotenv loader.
- The file stays excluded from backups and Git. Test injection and transfer by
  the same two checks above. Enable unattended execution only if that execution
  context can securely reach the file and destination after restart.

## Storage and first backup

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
Before resuming normal operations, reconcile repository locations with the
accepted shared registry.

For a new hub, create `paths.shared/registry.yaml` using this schema:

```yaml
version: 2
hub: default
max_bots: 50
bots:
  - id: first-bot
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
Use the configured `budget.max_bots` when initializing the shared limit.
For an existing hub, `setup` imports the current registry and registers the joining
Bot automatically. It preserves shared knowledge and migrates v1 hubs automatically.

**Confirm once.** At the end of the dialogue, show ASK answers and INFER results
in one concise list and get one confirmation. Include the current
`secrets.confirm_each_access` value; vendor approval still applies. Show
"Framework RAG is disabled; can enable on request" for the normal default;
honor any existing approved RAG state. Add one line: "Details are configurable
in YAML."

After the two preflight tests pass, prepare the allowlisted files. Before backup,
freeze the source and consistent SQLite exports, record the expected relative file
set (Bot: `data.include` minus built-in and configured exclusions), and retain a
private byte-for-byte copy outside all backup roots. For a new hub, also capture
`paths.shared` minus built-in exclusions, without applying `data.exclude`.
For a joining Bot, shared knowledge comes from the accepted hub state; use that
state's preserved source/inventory. Setup does not publish the joining Bot's local
authoring tree. If the original source is unavailable, record that source-byte
equality cannot be proved.
Record file sizes and hashes as an inventory, but retain the bytes for the
comparison. Do not diff an unfiltered live root or use a restore to build
its own expected baseline. Keep the selected sources quiescent, validate the local
configuration, invoke `setup`, and perform one `sync`; associate the inventory
with the selected snapshot IDs. Release the source freeze after capture/backup,
and keep the private expected copy until acceptance passes. Then delete that
temporary plaintext comparison copy; this cleanup does not delete original sources.

### Acceptance criteria

Before declaring setup complete or enabling scheduling, record all of:

- Secret injection and transfer tests passed for the selected execution context.
- `restic --repo SELECTED_REPOSITORY_URL check` succeeds for both the shared and
  Bot repositories, using the configured/acquired binary and authorized secret
  delivery. Coordinate the shared check with other users of the repository
  while other repository operations are paused: `check` takes an exclusive lock.
  Keep normal locking and never force-unlock to make the check proceed.
  Include `-o s3.region=auto` for R2. Use `check --read-data` to also verify
  stored payloads when the approved quota permits; never silently substitute it
  for the required restore comparison. Raw restic does not load `.env`; use the
  complete in-memory mapping in [SECRETS.md](SECRETS.md) and the
  [recovery commands](RECOVERY.md).
- Restore each selected complete snapshot by explicit ID into a fresh private
  directory. Ensure its parent exists, but do not create the destination itself:
  `restore` rejects existing paths, including empty directories.
  Compare the full expected relative file set and every file's bytes
  with the captured backup source/export, using `cmp` or an equivalent binary
  comparison. Include consistent database exports and unchanged shared knowledge
  and decision files. Validate the updated registry, generated `shared-revision.json`
  and `registry-history.json`, and any recorded migration decision supplement
  separately. Any other missing, extra, or changed file fails.
  `restore --verify` alone does not prove equality to the intended source.
  Keep timestamps, IDs, exit status, and comparison results; never log file
  contents or secrets.
- For Bot-managed transfer, retrieve the published encrypted generation into a
  fresh local directory and perform these checks on that copy. Local `sync`
  success does not satisfy remote backup acceptance. Preserve originals and the
  prior verified remote generation until this succeeds.

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
