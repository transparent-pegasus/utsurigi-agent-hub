# Optional utsurigi-memory

Install [utsurigi-memory.zip](utsurigi-memory.zip) only when framework RAG is enabled and the host selects
utsurigi-memory under the shared [review policy](../knowledge/practices.md). Existing instructions
still apply. Defaults: tasks, Caveman, and Bot-data sharing on; RAG off.

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

E5 requires separately acquired, checksum-verified weights and a compatible
runtime. Missing models fail clearly; `--hash` explicitly enables the weaker
legacy hash mode. The package README documents portable data paths, runtime
linking, model acquisition, verification, and `snapshot DESTINATION` for
consistent SQLite exports without inference.
