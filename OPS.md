---
description: Synchronization, publication, data scope, scheduling, and local cleanup.
when: Sync, backup, share, include/exclude, schedule, or cleanup requested.
---
# Operate

Run commands with `--config <bot YAML>`; inspect `status` JSON first.
`ready` means configuration fields are complete, not remote health or successful
setup. `next_action` and `remediation` guide the next step; timestamps describe
local operations. For any nonzero exit, read [RECOVERY.md](RECOVERY.md).

## Publish and consume

Only the current runtime publishes its Bot data. `registry publish` publishes the
owner's shared authoring tree; ordinary sync fetches shared knowledge without
publishing local policy edits. Proposals from other Bots await owner review and
the user's policy decision. Shared Bots place proposals in their owned tree for
owner pull; private Bots route proposals through the user. Consume only the verified
absolute version path in `status.active_shared`, never staging or an incomplete
import. Fetch peer data on demand with `pull BOT_ID --destination NEW_DIRECTORY`.

All Bots in one hub+machine use the same `paths.imports` root. Before restore,
compare the latest complete restic snapshot ID (SHA-256) with the validated cached
version; reuse an unchanged snapshot without restoring it again. A new snapshot
is restored and verified as `versions/<snapshotID>/`, then `CURRENT` switches
atomically. `status.active_shared` resolves that validated version's absolute path
for the hub+machine, rather than a Bot-specific copy. No symlink is required.
Keep only current and previous versions. Use `flock` to coordinate readers,
switching, and deletion; never remove a version still in use.

## Sync and schedule

Use `sync` for manual backup. Prefer native Bot scheduling to invoke
`sync --scheduled` often enough to service `status.next_due`.
The initial local-time anchor advances by fixed seconds, including across DST.
A successful scheduled sync advances the due time; manual sync keeps it. On
nonzero results, suspend automatic retries; inspect and reconcile before
resuming. A failed sync keeps its due time and does not authorize another write.
`sync.retry_lock_seconds` bounds restic lock waiting (default 120 seconds; zero
disables waiting), not whole-operation retry. A peer reader can delay completion
marking up to that bound.

Restic deduplicates data, but small SQLite changes may resend the entire database.
No per-record incremental transfer or fixed latency is guaranteed.

## Local cleanup and data scope

Local staging is plaintext and private to the host. After operations and readers
stop, remove crash-left `export-*` trees under `paths.state` if needed.
Protect current/previous shared versions and local originals. Repository history
cleanup requires owner-scoped `prune`; see [RECOVERY.md](RECOVERY.md).

Host exports live databases consistently and keeps exports quiescent during sync.
Declare SQLite exports in `data.sqlite_snapshots`; live files and WAL/SHM are not
backups. Secrets, Git internals, model weights, generated indexes, and logs remain
excluded. Keep local originals until successful synchronization is verified.

An explicitly included path that is excluded fails before repository writes.
Recursive exclusions warn with an escaped relative path and its owned/shared
source, then skip that subtree. These mandatory rules cannot be overridden.
Built-in rules match every path component case-insensitively:

- Exact names: `.git`, `.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`, `.docker`,
  `.netrc`, `.npmrc`, `.pypirc`, `gcloud`, `.secrets`, `secrets`, `private_keys`,
  `credentials`, `auth`, `authentication`, `auth_state`, `auth-state`, `sessions`,
  `cookies`, `.cache`, `__pycache__`, `node_modules`, `models`, `.models`,
  `model_weights`, `weights`, `index`, `indexes`, `.indexes`, `generated_index`,
  `generated_indexes`, `generated-index`, `index_cache`, `hnsw_index`, `indices`,
  `vector_index`, `vector-index`, `chroma`, `.chroma`, `.lancedb`, `logs`, `.env`,
  `model.bin`, `weights.bin`.
- Prefixes: `.env.`, `credentials.`, `secrets.`, `.secrets.`, `auth.`,
  `auth_state.`, `auth-state.`, `token.`, `tokens.`, `cookies.`, `logs.`, `id_rsa`,
  `id_dsa`, `id_ecdsa`, `id_ed25519`.
- Suffixes: `.env`, `.pem`, `.key`, `.p12`, `.pfx`, `.keystore`, `.gguf`, `.ggml`,
  `.safetensors`, `.onnx`, `.pt`, `.pth`, `.ckpt`, `.faiss`, `.index`, `.log`,
  `-wal`, `-shm`, `.wal`, `.shm`.
- Also excluded: names containing `.log.`, or starting with `pytorch_model` and
  ending in `.bin`.

`data.exclude` adds owned-data glob exclusions; a pattern without `/` also
matches at any depth. Matching directories exclude their entire subtree.
