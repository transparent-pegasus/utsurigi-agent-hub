---
description: Failures, restore, migration, handover, retirement, and credential rotation.
when: Any nonzero exit, restore/move/handover/retire/rotate, locks, wrong passwords, or interrupted setup.
---
# Recover

Stop scheduled retries after any nonzero exit. Inspect `status` JSON, including
`next_action` and `remediation`; readiness is configuration completeness, not
remote health. Reconcile uncertain repository writes before another write.
Keep originals and the last verified successful version.

## Restic diagnostics

Read the repository label (`shared` or `bot`), restic exit code, and sanitized first
stderr line. Never print raw diagnostics that could contain credentials.

| Restic exit | Meaning | Action |
|---|---|---|
| 3 | Unreadable source; incomplete snapshot | Correct source access; inspect history before retrying. |
| 10 | Repository missing | Check selected URL and mount; recover existing storage before initializing. |
| 11 | Repository locked | Check the holder; wait within the configured bound. Never force-unlock another process. |
| 12 | Incorrect password or no matching key | Verify the selected repository and authorized password reference; inject securely. |

Neither missing-repository nor password errors prove an empty destination. Check
the exact account, prefix/path, and mount before any initialization. If data exists
or its state is unknown, require the original password through the approved
[secret-delivery method](SECRETS.md); never generate a replacement to bypass an error.

`sync.retry_lock_seconds` bounds lock waiting, not operation replay. A lock or
password error does not prove an earlier write failed. Authentication,
configuration, and integrity failures need correction. Consult
[practices](knowledge/practices.md) for transport retry policy.

## Reconcile a backup

If `paths.state/pending-backup.yaml` exists, inspect its repository and `run` value
using the securely configured restic binary. Check for incomplete snapshots and
completion tags for Bot backups. For shared-v2 publication, compare the recorded run
with the accepted CAS head and verify the intended data; an unaccepted candidate
is never current authority. Preserve prior successful versions.
After reconciling that run and resolving its failure, remove only this record to
permit the next write. Do not blindly remove it to resume a schedule. Restore and
peer pull remain available for inspection while writes are held.

## Shared-cache migration

The new CLI does not treat a Bot's old random `status.active_shared` (`shared-*`)
path as authoritative. With an initialized schedule and no current cache, run
`sync` to import the cache and back up the selected Bot data; an uninitialized
schedule requires `setup`.
Old `shared-*` directories are not deleted automatically. Before cleaning them up,
stop old runtimes/readers, verify the new `status.active_shared` references, and
identify each cache as an old CLI artifact. Remove only those identified caches;
never delete arbitrary imports in bulk.

## Restore and handover

Before handover, preserve unfinished work and stop the old runtime. Update Bot
runtime identities using the current shared authoring base and shared credentials.
For an existing Bot's repository change, publish the updated registry entry before
running `setup` at the replacement repository.
For its own handover, the new runtime publishes with user-directed
`registry publish --takeover` after confirmation that the old runtime stopped.
Lost-runtime takeover also requires an explicit user decision; absence is not
consent. A returning superseded runtime keeps its local work for reconciliation.

`sharing.bot_data` records sharing intent; credentials determine access. Each private
Bot needs its own repository, password, environment name, and vault permissions.
Disabling prior sharing also needs a new repository/master key; a password change cannot revoke
the old key or erase what peers already read. Rotation checks may require the
publishing Bot to receive the affected old and new passwords through authorized secure
injection. Keep old recovery credentials for retained history.

Never reuse retired shared URLs, passwords, references/vault items, environment
names, or known master keys/IDs for private data; use fresh repositories initialized
by `setup`. Every complete encrypted shared snapshot carries cumulative shared
URLs, password references/environment names and observed restic repository IDs in
CLI-generated `registry-history.json`. This root filename is reserved: keep it out
of the authoring tree. Identifiers persist indefinitely, including after Bot
retirement and payload pruning. No passwords, keys or secret-derived
fingerprints are stored. Registration and private Bots' setup/sync reject known reuse.
Unavailable secret values under renamed references and unobserved foreign master
IDs still require host provenance checks; old peers' copies remain readable.

Complete shared snapshots with missing or invalid history cannot authorize
operations. Local caches or an ordinary restore do not replace that authority;
restore may recover bytes for inspection. Fleet recovery or relocation must
preserve the shared metadata. Automatic v1 migration requires valid existing
history; missing history is not reconstructed. Deleting all remote history loses
the guard. Shared credential holders are trusted publishers.

Publication may read delivered prior shared peers' repositories for identity
checks. Registered shared peers must run `setup` before subsequent checks, or the
host must withhold unneeded peer passwords. An unavailable prior repository
holds publication; exit 10 is not permission to forget its history.

Unrecognized snapshot metadata or a snapshot from another hub blocks operations on that
repository, including restore by a valid ID, until it is reconciled with
restic.

### Inspect a snapshot

Obtain snapshot IDs with the configured or setup-acquired restic binary, which
may be at `paths.state/bin/restic-<encryption.version>` rather than on PATH.
Invoke it with `--repo SELECTED_REPOSITORY_URL snapshots --json`, choosing the
shared or Bot repository. Use the same authorized secure delivery for its
password as `RESTIC_PASSWORD` and storage credentials; R2 also needs
`-o s3.region=auto`. Use a returned `id` with `restore`, choosing
`--repository shared` or `--repository bot`, plus
`--snapshot ID --destination NEW_DIRECTORY`. Restore into a fresh directory and
inspect it before adopting data. Restore accepts same-hub incomplete and
historical publisher snapshots for inspection; it does not make them current authority.
Its plaintext staging directory is created with mode 0700.
Restore leaves host-managed `user.hatch_tainted*` extended attributes untouched;
other extended attributes and content verification retain normal restic behavior.

Run `restic --repo SELECTED_REPOSITORY_URL check` with the same binary, secure
delivery, and R2 option before accepting recovery. Coordinate a shared-repository
check while other repository operations are paused; `check` takes
an exclusive lock. Keep normal locking and never force-unlock another operation.
Before checking a Bot repository, pause that Bot's scheduled sync, let active
sync/pull operations finish, and coordinate with peers to keep pulls paused for
the duration of the check.
`check --read-data` additionally
reads the stored payloads; account for its transfer cost. See the
[pinned restic integrity-check guide](https://github.com/restic/restic/blob/v0.19.1/doc/045_working_with_repos.rst).
Raw restic does not load the Hub's config-adjacent `.env`; the trusted host launcher
must map the selected `password_env` to `RESTIC_PASSWORD` without displaying it
or shell-sourcing the file. It must also supply R2's AWS credentials and the
route's proxy/CA settings, as described in [SECRETS.md](SECRETS.md).

For setup acceptance or a recovery drill, compare the restored relative file set
and every source file's bytes against the quiescent source/export captured for that
snapshot. Validate updated registry/revision/history metadata and any recorded
migration decision supplement separately; unchanged knowledge and existing decision
bytes must match. Other missing, extra, or different files fail. `restore --verify` alone is
insufficient. If the original is lost, report that source-byte equality cannot be
retested; successful integrity verification is not evidence of that comparison.
For Bot-managed remote transfers, fetch a complete verified encrypted generation
into a fresh directory and check/restore that copy. Retain the previous generation
and originals until all applicable [acceptance criteria](INIT.md#acceptance-criteria)
pass. Delete the temporary plaintext comparison copy after successful acceptance;
keep the original sources according to their approved retention policy.

Incomplete/uncertain writes never authorize deletion of the prior successful
version. With `sync.versioning: true` (default), a completed predecessor expires
only after its unique successful successor reaches `sync.history_retention_seconds`
in age; cleanup requires explicit `prune`. With `false`, sync or shared publication
removes only pre-write completed predecessors after the new backup is verified and
local publication succeeds. The new backup remains if cleanup fails, and the
pending-write guard blocks another write until reconciliation. Incomplete or
uncertain snapshots remain available for inspection. These settings expire Bot
payload versions, not the cumulative identifier guard. Never force-unlock another
process. This version retains shared v2 states and migration captures; shared
storage can grow and reach a budget hold.

`setup` migrates a valid v1 hub automatically. It preserves source data in a retained
snapshot and records a format marker that makes old CLI shared operations stop.
After interruption, retry `setup` with the new CLI; it resumes from the recorded
capture. Keep the format marker, capture, and CAS head. Unknown or corrupt history
requires recovery; the CLI never silently falls back to v1.
