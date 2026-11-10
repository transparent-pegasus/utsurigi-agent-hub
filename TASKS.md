---
description: Shared task format, progress reads, dependencies, logs, and native migration.
when: Recording work, checking progress, waiting for user action, goals, or migrating tasks.
---
# Shared tasks

Task management is required for every Bot, including existing installations.
Native scheduling, routines and hooks remain in the Bot. Keep their implementation
there and store only references here. Preserve native records until migration
has been compared and the shared publication has succeeded.

## Files and reads

The authoring store is `paths.shared/tasks/`. `FORMAT` identifies version 1;
`INDEX.md` contains **only `in_progress`** tasks. Each task has one UUID-named
Markdown file. Open tasks live directly in this directory; completed and cancelled
tasks move to `archive/<UUID>.md`. Reopening moves the same file back. Never delete
closed tasks to hide them from progress reads.

For normal Bot progress checks, use `tasks list --accepted`. It reads the verified
shared cache's INDEX and only the listed `in_progress` files. It does not open
pending/waiting files or archived bodies. Queued work and waits require explicit
selection: `tasks list --accepted --status pending`, `--status waiting`, or
`--status waiting_user`. Use `--all` only when history or a full inventory is needed.
An empty progress list does not mean there is no queued or waiting work.

Without `--accepted`, list/show use the local authoring tree. These can contain
unpublished edits. Accepted reads hold the existing shared-cache reader lock;
they use the last verified cache generation, not a live remote query. Refresh via
the normal sync protocol when freshness matters. Raw reads of the cache follow
the [OPS reader lock protocol](OPS.md#publish-and-consume).

```yaml
---
id: 3f2a9c1e-0000-4000-8000-000000000001
parent: null
title: 4体へのuah導入
status: waiting_user
owner: shotoku-taishi
schedule: null
due: 2026-11-15T23:59+09:00
workdir: /workspace/uah-intro-4bots
requires:
  - repo: transparent-pegasus/utsurigi-agent-hub
  - secret: utsurigi-r2-credentials
start_after: []
done_when: []
updated: 2026-11-10T05:21+09:00
---
## Next
4体への通知文を承認してもらう

## Note
registry rev8、全員shared。

## Log
- 2026-11-10T05:21+09:00 | shotoku-taishi | 通知文を用意し、ユーザーの承認待ちにした。
```

IDs are canonical lowercase UUIDs; references must resolve within this store.
`parent`, `owner`, `schedule`, `due`, and `workdir` may be null. `requires`,
`start_after`, and `done_when` default to empty lists. Dates include a timezone;
seconds or the minute precision shown above are accepted. `updated` is refreshed
by successful writes. Unknown metadata keys are rejected.

`requires` contains nonempty `repo` or `secret` names, never secret values.
`schedule` is null or `{bot: BOT_ID, id: NATIVE_SCHEDULE_ID}`. It is an opaque
reference, not cron syntax or code to execute. `due` does not authorize execution.
`workdir` is an owner's reference; other machines must resolve access themselves.

Put long-term outcome and acceptance criteria in the goal task's `## Goal` section,
before `## Next`. Use `parent` for subgoals, `done_when` for completion requirements,
and child tasks for multiple native schedules. Preserve an existing workdir,
GOAL.md, and activity logs by reference or through an authorized text export.
The CLI does not read or publish the contents of referenced paths automatically.

`## Log` follows `## Note`. Every work entry records the Bot name and what it did;
include an explicit-timezone timestamp, for example `- TIMESTAMP | BOT_ID | CONTENT`.
Append concrete results and decisions instead of erasing previous work. Create,
put, import, and propagated cancellation append an attributed event automatically.
Additional work details belong in the same Log. Existing Markdown text is preserved.

## Status and dependencies

| Status | Meaning |
|---|---|
| `pending` | Queued; check prerequisites/resources before starting |
| `in_progress` | Work is active; included in normal progress reads |
| `waiting` | Waiting for an external event or service |
| `waiting_user` | Waiting for the user to do something or decide |
| `completed` | Finished; stored in archive |
| `cancelled` | Stopped; stored in archive |

An open task may move to any state. Reopen completed/cancelled tasks through
`pending`; previously cancelled children stay cancelled until explicitly reopened.
When reopening an accepted closed task, publish `pending` before moving it to
another status, so the shared transition is checked against that reopened state.
Starting requires every `start_after` task to be completed. Completion additionally
requires every `done_when` task completed. Cancellation never counts as completion.
There is no automatic goal completion: meeting dependencies enables the explicit
completion transition. Resources still require host checks.

`tasks show UUID --accepted` reports `ready`, `blockers`, and
`completion_blockers`. `ready` is false for waits and closed tasks. It describes
task state and start dependencies, not resource availability or the native clock.
Before each native scheduled action, refresh the accepted generation or verify
that the cache is still current, then check this task,
its schedule reference, prerequisites and resources. A blocked/cancelled task must
not run. This cannot forcibly stop an external action that is already running.

Parent edges and the combined `start_after`/`done_when` graph are each checked for
cycles using an iterative traversal. Do not combine parent edges with wait edges:
a goal waiting for a child that names that goal as parent is valid. Self-waits,
missing IDs, parent loops and wait cycles fail before mutation/publication.

Cancelling a parent cancels every unfinished descendant, including user waits.
Completed descendants retain their history. The whole change, archive moves and
Log entries use one recoverable transaction. Tasks connected only by dependencies
retain their own status and remain blocked by unmet requirements. They may be
rewired, resumed after prerequisites succeed, or explicitly cancelled. Reopening
a prerequisite does not undo another task's completion or restart cancelled work.
Cancelled requirements propagate execution blockers through both kinds of wait
edges, including indirect `done_when` chains. An ordinary unfinished `done_when`
requirement only blocks completion; it does not prevent useful work from starting.

## Commands and concurrent updates

Use the normal `utsurigi-agent-hub --config BOT_YAML` prefix:

```sh
utsurigi-agent-hub --config BOT_YAML tasks init
utsurigi-agent-hub --config BOT_YAML tasks create 'Prepare migration'
utsurigi-agent-hub --config BOT_YAML tasks show UUID
utsurigi-agent-hub --config BOT_YAML tasks put --file TASK_DRAFT.md --if-match ETAG
utsurigi-agent-hub --config BOT_YAML tasks validate
utsurigi-agent-hub --config BOT_YAML tasks index
utsurigi-agent-hub --config BOT_YAML registry publish
```

Create starts in `pending`. Show returns metadata, body, and the SHA-256 ETag of
the exact Markdown bytes. For put, copy the task and body into a draft, edit it,
and supply the last ETag; creating a new UUID omits `--if-match`. `bot.id` supplies
Log attribution. All authorized Bots may edit; `owner` is responsibility metadata.

Task commands serialize on `tasks/operation.lock`. Do not replace the directory
or lock while commands are running. Atomic file replacement and `.pending.json`
provide roll-forward recovery for batches and archive moves. After interruption,
the next authoring command/export finishes the journal before reading; malformed
journals stop for reconciliation. Preserve them and the original export; never
blindly delete a pending journal. Manual file edits require quiescent writers,
correct archive placement, and `tasks index` before publishing.

Cross-machine writes use the existing shared revision CAS. Refresh the verified
authoring base as described in OPS, retain `shared-revision.json`, omit generated
`registry-history.json`, then reconcile edits and publish. A stale base or lost
CAS preserves drafts and requires explicit reconciliation. Ordinary sync consumes
shared tasks; it does not publish local edits. Accepted publications validate
the graph, status transitions, cancellation propagation, INDEX and retained history.

## Migration from GrokBot and Muse

Inventory authorized native records first, including queued work, waits, goals,
subgoals, activity logs and linked schedules. Export task data without secret
values or scheduler bodies. Do not infer completion from missing records.
Use stable native IDs unique within each Bot. For conversation checklists without
durable IDs, include conversation identity and a stable step key, not its list position.
For an ephemeral Muse checklist, explicitly complete its task when the step is
finished; disappearing from the old checklist must not delete the shared task.

Grok's JSON list keeps its three native fields; export IDs as strings:

```json
[{"id":"1","content":"Prepare rollout","status":"pending"}]
```

Use `tasks import grok --file EXPORT.json`. Its four native statuses are preserved;
multiline content remains in Next, with the first nonempty line used as title.

Muse's native host produces a flat interchange list; this is an export contract,
not a claim that Muse exposes a particular API:

```json
[
  {"id":"g","kind":"goal","title":"Rollout","status":"in_progress",
   "workdir":"/workspace/rollout","goal":"GOAL.md text or reference",
   "activity":"2026-11-10: began","done_when":["step"]},
  {"id":"step","kind":"checklist","title":"Prepare notice","status":"pending",
   "parent":"g","next":"Draft notice","note":"Preserve context"},
  {"id":"delivery","kind":"tracking","title":"Delivery","status":"waiting_user",
   "start_after":["step"],"due":"2026-11-15T23:59+09:00"},
  {"id":"routine","kind":"schedule","title":"Daily check","status":"pending",
   "parent":"g","schedule":"native-routine-12"}
]
```

Use `tasks import muse --file EXPORT.json`. Required fields are `id`, `kind`,
`title`, and `status`. Kinds are `checklist`, `tracking`, `goal`, and `schedule`.
Optional fields are `parent`, `schedule`, `due`, `workdir`, `requires`, `start_after`,
`done_when`, `next`, `note`, `goal`, and `activity`. Schedule records require a
nonempty native schedule ID. Relationships use native IDs; import resolves them
to UUIDs across the entire batch, including previously imported records. Goals go
to Goal, steps to Next/Note, and activity is retained under an attributed Log entry.

The result returns native-ID-to-UUID mappings. Immutable `source: {system, bot, id}`
metadata makes repeated imports idempotent and keeps different Bots' native IDs
separate. Reimport never overwrites later shared edits or removes absent records.
Unsupported input, missing references, or cycles reject the entire batch.
Quiesce native task edits for the final export and comparison. Reimport deduplicates;
reconcile changes since an earlier import with put/ETag before switching systems.

Compare every imported source ID, state, relationship, body/log and schedule
reference, including archived records. Publish, verify an accepted read from
another Bot, then retire duplicate native task display/tracking. Retain recovery
exports. Native routines, cron/hooks and their implementations remain active in
their Bot with the task guard above. There is no live service migration or native
deletion command in this CLI.
