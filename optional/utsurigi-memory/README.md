# utsurigi-memory

Optional text memory: SQLite storage and cosine search with multilingual E5 or
explicit character hashing. Prefer the host's native memory. Installing this
package does not enable framework RAG or change existing instructions or tasks.

## Build

Requires Unix sockets, Bash, Rust with `std::fs::File::lock`, and C/C++ build tools.
No executable, model weights, database, or ONNX Runtime library is bundled.

On Debian/Ubuntu, install `g++`: `sudo apt install g++`. This supplies the C++
compiler and libstdc++ development files needed for linking; `gcc` alone is
insufficient. ONNX Runtime also needs libstdc++ at runtime.

```sh
bash build.sh
```

This installs `~/.local/bin/utsurigi-memory`; `bash utsurigi-memory ...` builds
it if missing. Build artifacts use `CARGO_TARGET_DIR`, or
`$XDG_CACHE_HOME/utsurigi-memory-target`, falling back to
`~/.cache/utsurigi-memory-target`. The locked dependencies are retained.

The default build downloads ONNX Runtime binaries through `ort-sys`. Those
static binaries can require newer glibc/C++ symbols than the host provides
(including `__isoc23_strtoll` on glibc 2.36). If linking fails, acquire the official
ONNX Runtime 1.24.2 CPU release for your platform and use its `lib` directory:

```sh
export ORT_LIB_PATH="/path/to/onnxruntime/lib"
export ORT_PREFER_DYNAMIC_LINK=1
export LD_LIBRARY_PATH="$ORT_LIB_PATH${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export RUSTFLAGS="-C link-arg=-Wl,-rpath,$ORT_LIB_PATH${RUSTFLAGS:+ $RUSTFLAGS}"
bash build.sh
```

The rpath also supports test children that clear their environment. Keep the
dynamic library at that location when running `utsurigi-memory`. This override was
verified on Linux x86_64; the default static build was not verified there.
Use [official releases](https://github.com/microsoft/onnxruntime/releases/tag/v1.24.2).

## Commands and paths

```sh
utsurigi-memory --hash add "Remember the project decision" --tag project
utsurigi-memory --hash add "Imported decision" --created-at "2023-11-15T07:13:20+09:00"
utsurigi-memory --hash search "project decision" -k 5
utsurigi-memory --json list
utsurigi-memory delete 1
utsurigi-memory info
utsurigi-memory snapshot /path/to/new-snapshot.db
```

`UTSURIGI_MEMORY_HOME`, `UTSURIGI_MEMORY_PATH`, `UTSURIGI_MEMORY_CACHE`,
`UTSURIGI_MEMORY_SOCK`, and
`UTSURIGI_MEMORY_IDLE_SECS` override their defaults.
Empty values and empty XDG variables mean unset. `UTSURIGI_MEMORY_NO_DAEMON`
is the exception: its presence disables the daemon, even with an empty value.

The default data/model directory is `$XDG_DATA_HOME/utsurigi-memory`, falling back
to `$HOME/.local/share/utsurigi-memory`. The default database is `memory.db` inside
the selected `UTSURIGI_MEMORY_HOME` or default data/model directory.
`UTSURIGI_MEMORY_PATH` selects a database file; `--db FILE` overrides it and rejects
an omitted or empty FILE value.
Selected `UTSURIGI_MEMORY_PATH` and `UTSURIGI_MEMORY_SOCK` values must be UTF-8.
The wrapper preserves these choices. All stored timestamp displays use UTC.

`add --created-at RFC3339` retains an imported memory's creation time, including
timezone offsets. Storage remains UTC Unix seconds; fractional seconds are
discarded. Omit the option to use the current time. Missing, invalid, duplicate,
or non-add uses fail before model loading or database changes. JSON `list` and
`search` emit `created_at` as RFC3339 UTC with seconds (for example,
`2023-11-14T22:13:20Z`), accepted unchanged by `add --created-at`. Human output
keeps its minute-only UTC format.

`list` exits successfully without diagnostics when its output pipe closes early,
including with `--json`. Other output errors remain failures.

Options may precede or follow the command. Unknown, duplicate, missing, empty,
invalid numeric, and command-inappropriate options fail before database access.
`--tag` and `--created-at` are add-only, `-k` is search-only, and `--idle` is
serve-only. Use `--` before literal text beginning with an option name.
`info --json` returns `db`, `embedder`, and `count`; `stop --json` returns
`stopped` (false when no daemon responds).
SQLite tables `meta` and `memories` and their stored vector format are unchanged.
The first embedding pins `multilingual-e5-small` (384 dimensions) or
`char-ngram-hash-1024` (1024 dimensions). Different spaces cannot be mixed.
Existing hash databases require `--hash` for add/search. Invalid vector sizes,
non-finite values, and unreadable rows produce errors; `info` uses SQL COUNT.

`info`, `list`, `search`, and `snapshot` open existing databases read-only. Missing
sources fail without creating databases or parent directories. Reads never
initialize the schema or pin an embedder; `add` pins the first embedding space.
`delete` opens an existing database for writes; it creates neither missing
databases nor their parent directories.
Check the destination with `info` before migration: a missing database stays
absent, so a later snapshot can use that path.

`snapshot DESTINATION` uses SQLite `VACUUM INTO` for a consistent export,
including committed WAL data. It needs no model or
daemon, preserves records, and refuses every existing destination, including an
empty file. It creates the destination with Unix mode `0600`, restricted further
by the caller's umask. After opening the source read-only, it creates missing
destination parent directories. A missing source leaves source and destination
paths untouched.
Check a snapshot before adopting or backing it up.

## Model and daemon

There are **no model weights in this archive**. E5 add rejects missing, invalid,
or checksum mismatched models before creating or writing a database or binding a
new daemon socket. Search opens its existing database read-only before inference.
Hashing is used only with explicit `--hash`; it is a weaker keyword-like mode.
`list`, `delete`, `info`, and `snapshot` need no inference.

Only when RAG is enabled and E5 is selected, acquire the compatible fp32 model:

```sh
model_dir="${UTSURIGI_MEMORY_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/utsurigi-memory}/model/multilingual-e5-small"
mkdir -p "$model_dir"
cp model/multilingual-e5-small/*.json model/multilingual-e5-small/model.onnx.sha256 "$model_dir/"
curl --fail --location --output "$model_dir/model.onnx" \
  https://huggingface.co/intfloat/multilingual-e5-small/resolve/614241f622f53c4eeff9890bdc4f31cfecc418b3/onnx/model.onnx
(cd "$model_dir" && sha256sum --check model.onnx.sha256)
```

The pinned model SHA256 is
`ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665`.
Its tokenizer/config files are included; see [MODEL-NOTICE.md](MODEL-NOTICE.md).
Keep the checksum file beside the weights. `model.onnx.partNN` files are joined
in numeric suffix order, with or without zero padding (`part2` precedes `part10`).
Malformed suffixes and duplicate numbers such as `part1` and `part01` fail. Direct, joined, and
cached weights are all checksum verified. Remove an invalid cached model before
retrying. E5 uses mean pooling, `passage: ` for stored text, `query: ` for searches,
and L2 normalization. It is not a multimodal model.

E5 add/search auto-start `utsurigi-memory serve`. The daemon holds the model
and exits after `UTSURIGI_MEMORY_IDLE_SECS` of inactivity (default 600).
An invalid environment value uses 600; `serve --idle SECS` overrides it.
`utsurigi-memory status` leaves the idle
timer unchanged; `utsurigi-memory stop` stops it. Neither command creates a missing
cache directory. Database operations stay in
the CLI. `UTSURIGI_MEMORY_NO_DAEMON` embeds in process whenever present;
`--hash` always runs in process.

`UTSURIGI_MEMORY_CACHE` overrides `$XDG_CACHE_HOME/utsurigi-memory` or
`$HOME/.cache/utsurigi-memory`. It holds `utsurigi-memory.sock`,
`utsurigi-memory.lock`, `utsurigi-memory.log`, and a joined `model.onnx`.
`UTSURIGI_MEMORY_SOCK` overrides the socket path. Socket paths must fit the platform's
byte limit (107 bytes on Linux), including multibyte characters. Invalid paths
fail before connecting, loading models, or starting a daemon. Set a short,
deployment-specific socket path when the cache path is too long.
Use a local filesystem for this directory and keep separate caches/sockets for
separate model deployments.

## Verify

```sh
cargo fmt --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

Use the runtime environment above when needed.
