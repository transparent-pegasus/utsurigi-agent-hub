# Optional utsurigi-memory

Install [utsurigi-memory.zip](utsurigi-memory.zip) only when framework RAG is enabled and the host selects
utsurigi-memory under the shared [review policy](../knowledge/practices.md). Existing instructions
still apply. Template defaults: tasks, Caveman, and Bot-data sharing on; RAG off.
Setup overrides duplicate optional features to `false` when they supply no
capability missing from the Bot; an explicit user decision of `true` wins.
Bot-data sharing is a separate choice. See [INIT](../INIT.md#optional-capabilities).

Reuse a compatible shared memory store within the selected hub before installing
another. Keep shared `UTSURIGI_MEMORY_HOME` and `UTSURIGI_MEMORY_CACHE` outside Bot
workspaces; record them in `memory.existing_source`. Share a live store and cache
only on one machine's local filesystem within that hub. Reusing
`UTSURIGI_MEMORY_HOME` across hubs requires an explicit per-hub
`UTSURIGI_MEMORY_PATH` (or separate HOME), isolated cache/socket paths, and
separate native stores. Immutable model assets and common code may be reused;
the memory CLI remains standalone. Separate private stores and model deployments.
Let the initializing Bot check machine identity, access, and sharing before reuse.

Extract into a new directory and read `utsurigi-memory/README.md` before running
`bash utsurigi-memory/build.sh`. The package contains source, scripts, a dependency
lockfile, and tokenizer/config assets with provenance. It contains no model
weights, database, compiled executable, or ONNX Runtime library.

On Debian, install C++ build tools before building:

```sh
apt install g++
```

This supplies the development `libstdc++` linker library. Having `gcc` and
`libstdc++.so.6` alone can still produce `unable to find library -lstdc++`.

E5 requires separately acquired, checksum-verified weights and a compatible
runtime. Missing models fail clearly; `--hash` explicitly enables the weaker
legacy hash mode. The package README documents portable data paths, runtime
linking, model acquisition, verification, and `snapshot DESTINATION` for
consistent SQLite exports without inference.

## Paths and recovery

Use a short private socket path via `UTSURIGI_MEMORY_SOCK` when the cache path is
long. Linux socket paths must fit in 107 bytes, including directory components;
the CLI checks length before model loading or daemon startup and reports an error.
Do not assume an automatic fallback. Keep sockets local and isolated across hubs
and private stores; recheck paths when moving machines.

`info`, `list`, and `search` open existing databases read-only. `delete` also
requires an existing database. A missing source fails without creating a database
or its parent directory. Reading a recovery destination must not create an empty
database that blocks `snapshot DESTINATION`.
Snapshot destinations must not exist, even as empty files. `snapshot DESTINATION`
creates missing destination parents only after opening the source database.
Snapshot output files are created with mode 0600.
Inspect the snapshot before adoption; retain the source until recovery is verified.

Prefer snapshots when moving a database: records and timestamps survive together.
For deliberate record-by-record migration, preserve each original creation time:

```sh
utsurigi-memory --hash add "Project decision" --tag project --created-at "2026-10-31T00:00:00Z"
```

`list`, `search`, and `info` exit quietly on a broken output pipe (EPIPE),
including when piped to `head`. This also applies to `list --json` and
`search --json`; other output errors remain failures.

`list --json` emits RFC3339 `created_at` timestamps with seconds, accepted by
`add --created-at`. The stored value is UTC Unix seconds. Omit `--created-at` for
the current time. Use `--hash` only for hash databases; retain the selected
embedding model for E5. Do not mix embedding spaces or replace an existing native
store.
