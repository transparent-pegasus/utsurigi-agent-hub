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

`sync.retry_lock_seconds` bounds lock waiting, not operation replay. A lock or
password error does not prove an earlier write failed. Authentication,
configuration, and integrity failures need correction. Consult
[practices](knowledge/practices.md) for transport retry policy.

## Restore and handover

Before handover, preserve unfinished work and stop the old runtime. The owner
updates Bot runtime identities in the registry. For the registry owner's own
handover, the new runtime publishes with user-directed
`registry publish --takeover` after confirmation that the old runtime stopped.
Lost-runtime takeover also requires an explicit user decision; absence is not
consent. A returning superseded runtime keeps its local work for reconciliation.

`sharing.bot_data` controls peer access only within the selected hub. Each private
Bot needs its own repository, password, environment name, and vault permissions.
Disabling prior sharing also needs a new repository/master key; a password change cannot revoke
the old key or erase what peers already read. Rotation checks may require the
owner to receive the affected old and new passwords through authorized secure
injection. Keep old recovery credentials for retained history.

Never reuse retired shared URLs, passwords, references/vault items, environment
names, or known master keys/IDs for private data; use fresh repositories initialized
by `setup`. Every complete encrypted shared snapshot carries cumulative shared
URLs, password references/environment names and observed restic repository IDs in
CLI-generated `registry-history.json`. This root filename is reserved: keep it out
of the authoring tree. Identifiers persist indefinitely, including after Bot
retirement, owner changes and payload pruning. No passwords, keys or secret-derived
fingerprints are stored. Admission and private Bots' setup/sync reject known reuse.
Unavailable secret values under renamed references and unobserved foreign master
IDs still require host provenance checks; old peers' copies remain readable.

Complete shared snapshots with missing or invalid history cannot authorize
operations. Local caches or an ordinary restore do not replace that authority;
restore may recover bytes for inspection. Fleet recovery or relocation must
preserve the shared metadata. This fresh format has no automatic migration from
snapshots without it; deleting all remote history loses the guard. Authorized
shared writers are trusted, not defended against as malicious publishers.

Publication may read delivered prior shared peers' repositories for identity
checks. Admitted shared peers must run `setup` before subsequent checks, or the
host must withhold unneeded peer passwords. An unavailable prior repository
holds publication; exit 10 is not permission to forget its history.

Any unmarked snapshot or snapshot from another hub blocks operations on that
repository, including restore by a valid ID, until the owner reconciles it with
restic.

Obtain snapshot IDs with the configured or setup-acquired restic binary, which
may be at `paths.state/bin/restic-<encryption.version>` rather than on PATH.
Invoke it with `--repo SELECTED_REPOSITORY_URL snapshots --json`, choosing the
shared or Bot repository. Use the same authorized secure delivery for its
password as `RESTIC_PASSWORD` and storage credentials; R2 also needs
`-o s3.region=auto`. Use a returned `id` with `restore`, choosing
`--repository shared` or `--repository bot`, plus
`--snapshot ID --destination NEW_DIRECTORY`. Restore into a fresh directory and
inspect it before adopting data.

Incomplete/uncertain writes never authorize deletion of the prior successful
version. A completed predecessor expires only after its unique successful
successor reaches `sync.history_retention_seconds` in age. That setting expires
payload versions, not the cumulative identifier guard. Cleanup requires explicit,
owner-scoped `prune`; never force-unlock another process.

After an interrupted initial owner setup, retry is allowed only when every
existing shared snapshot is incomplete and belongs to that same Bot and runtime.
The interrupted snapshots remain; a fresh backup must complete normally. Unknown
or foreign history requires explicit host recovery.
