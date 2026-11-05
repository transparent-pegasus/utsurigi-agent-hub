---
description: Synchronization, publication, data scope, scheduling, and local cleanup.
when: Sync, backup, share, include/exclude, schedule, or cleanup requested.
---
# Operate

Run commands with `--config <bot YAML>`; inspect `status` JSON first.
`ready` means configuration fields are complete, not remote health or successful
setup. `joined` reports completed setup/own sync. `failure` reports an unresolved
operation failure, or `null`; follow its reason and the `remediation` strings.
`next_action` names the operation to address, `configure`, `reconcile`, or `wait`.
`remediation` is an array of strings; timestamps describe local operations.
`anchor`, `last_success`, and `next_due` in status JSON are RFC3339 strings with
seconds in UTC (`YYYY-MM-DDTHH:MM:SSZ`), or `null` when unset. Persisted schedule
state keeps Unix seconds.
For any nonzero exit, read [RECOVERY.md](RECOVERY.md).

## Publish and consume

Only the current runtime publishes its Bot data. `registry publish` publishes the
shared authoring tree using shared credentials. Copy the current verified shared
version, retain `shared-revision.json`, omit generated `registry-history.json`, then edit. Stale publication fails
and preserves local edits.
Ordinary sync fetches shared knowledge without publishing local policy edits.
Review proposals before applying the user's policy decision. A Bot with the
target repository's credentials can pull proposals; otherwise route them through
the user. Consume only the verified
absolute version path in `status.active_shared`, never staging or an incomplete
import. Fetch peer data on demand with `pull BOT_ID --destination NEW_DIRECTORY`.
If the latest complete peer snapshot's Bot/runtime/kind differs from the registry,
pull stops with an error. Use `--allow-identity-mismatch` only on an explicit user
instruction to inspect that version. Hub checks and destination validation still apply.

All Bots in one hub+machine use the same `paths.imports` root. The managed root is
`paths.imports/<SHA-256(hub)>/`. It contains `versions/<snapshotID>/`, JSON `CURRENT`
(`hub`, `current`, `previous`), and `operation.lock`.
Before restore, compare the accepted CAS head's full snapshot ID with
the validated cached version; reuse a matching ID without restoring it again.
A new snapshot is restored and verified under `versions/<snapshotID>/`, then
`CURRENT` switches atomically. `status.active_shared` resolves that validated
version's absolute path for the hub+machine. Keep current and previous versions.

Hosts/Bots must hold a shared `flock` on the managed root's `operation.lock`
throughout long reads. After acquiring it, reread `status.active_shared` and use
that version. Without this coordination, an old version path cannot be retained
indefinitely.

Cache hits take a shared lock alongside readers. Cache mutations (restore,
verification, switching, and garbage collection) need the exclusive lock and wait
up to 30 seconds. The CLI releases the cache lock before Bot-data upload or
unrelated work. Lock errors name the affected lock path.

This cache-importing sync also backs up the selected Bot data.

## Sync and schedule

With `joined: true`, an initialized schedule without a current shared cache gives
`next_action: sync`. `joined: false` or an uninitialized schedule requires `setup`.

Use `sync` for manual backup. Prefer native Bot scheduling to invoke
`sync --scheduled` often enough to service `status.next_due`.
The initial local-time anchor advances by fixed seconds, including across DST.
A successful scheduled sync advances the due time; manual sync keeps it. On
nonzero results, suspend automatic retries; inspect and reconcile before
resuming. A failed sync keeps its due time and does not authorize another write.
Before sync/publication writes, the CLI saves `paths.state/pending-backup.yaml`.
It clears this record only after remote verification and local publication succeed.
While it exists, setup/sync/publication/prune stop; follow [reconciliation](RECOVERY.md#reconcile-a-backup).
`sync.retry_lock_seconds` bounds restic lock waiting (default 120 seconds; zero
disables waiting), not whole-operation retry. A peer reader can delay completion
marking up to that bound.

Restic deduplicates data, but small SQLite changes may resend the entire database.
No per-record incremental transfer or fixed latency is guaranteed.

## Local cleanup and data scope

Local staging is plaintext and private to the host. After operations and readers
stop, remove crash-left `export-*` trees under `paths.state` if needed.
Protect current/previous shared versions and local originals. Repository history
cleanup requires explicit `prune`; see [RECOVERY.md](RECOVERY.md).

Host exports live databases consistently and keeps exports quiescent during sync.
Declare SQLite exports in `data.sqlite_snapshots`; live files and WAL/SHM are not
backups. Secrets, Git internals, model weights, generated indexes, and logs remain
excluded. Keep local originals until successful synchronization is verified.
Use the [setup acceptance criteria](INIT.md#acceptance-criteria) for the first
backup and [recovery drills](RECOVERY.md): `restic check`, restore, and byte-for-byte
comparison to the captured source/export. Bot-managed transfer also needs
verification of a freshly retrieved remote generation. A successful local sync
does not prove that.
Keep fallback `.env` files beside private configs, outside data roots; remove
upload copies as specified in [INIT](INIT.md#attached-env-fallback).

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
  `vector_index`, `vector-index`, `chroma`, `.chroma`, `.lancedb`, `logs`,
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
