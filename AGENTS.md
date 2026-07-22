# AGENTS.md — BlossomFS Maintenance Guide

> **Read this before making any changes to blossomfs.**

## Project Description

BlossomFS is a FUSE filesystem that mounts [Blossom](https://github.com/hzrd149/blossom) /
Nostr blob storage as a local directory tree. It speaks the BUD-01/02/03/04/06/08/11/12
protocols, lazily fetches blobs on first `read()`, verifies every download against its
SHA-256, and serves subsequent reads from a content-addressed on-disk cache. It supports
both read-only (default) and read-write modes — RW mode buffers writes in memory and
uploads them to a Blossom server on `flush()`.

- **Crate**: `blossomfs` v0.1.0, Rust edition 2024
- **License**: MIT
- **Repo**: https://github.com/Amperstrand/blossomfs
- **FUSE binding**: `fuser` 0.17 (callbacks take `&self` — all mutable state uses interior
  mutability via `Arc<RwLock<Tree>>` and `Arc<Mutex<HashMap<…>>>`)

## Architecture

Four layers, each a module under `src/`, plus two cross-cutting concerns:

```
┌─────────────────────────────────────────────┐
│   CLI (cli.rs) · clap parsing, startup      │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│   FUSE Layer (fuse/)                         │
│   fs.rs · tree.rs · inode.rs · vfiles.rs     │
│   fuser::Filesystem trait impl               │
│   Virtual directory tree, inode allocation   │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│   Blossom Client (blossom/)                  │
│   client.rs · descriptor.rs · auth.rs ·      │
│   manifest.rs                                │
│   Hybrid: nostr-blossom types + raw reqwest  │
│   BUD-01/02/03/04/06/08/11/12 interactions   │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│   Nostr Layer (nostr/)                       │
│   keys.rs · discovery.rs · nip94.rs ·        │
│   nip34.rs · legacy_drive.rs · persist.rs ·  │
│   auth.rs · tollgate.rs                      │
│   Key parsing, relay queries, kind parsing   │
└─────────────────────────────────────────────┘

Cross-cutting:
  Cache (cache/)    — object_cache.rs, fetch.rs (content-addressed blob store)
  Util  (util/)     — path.rs (sanitization), mime.rs (extension inference)
  metrics.rs        — Prometheus endpoint
  control.rs        — Unix-socket control protocol
  config.rs         — figment hierarchical config (CLI > env > TOML file)
  payment.rs        — Cashu / NWC payment strategies
  git/              — NIP-34 lazy git clone browser
```

### Bootstrap sequence (`main.rs`)

1. Install rustls `ring` crypto provider.
2. Init `tracing_subscriber` with env filter (default `info`).
3. Parse CLI via clap; merge config (TOML file + `BLOSSOMFS_*` env vars) into `MountArgs`.
4. Optionally daemonize (`--daemon`).
5. Create a tokio multi-threaded runtime (lives for the whole process; its `Handle` is
   cloned into `BlossomFS` so sync FUSE callbacks can `block_on` async work).
6. Build the virtual `Tree`: discover servers via NIP-B7 (kind 10063), list blobs via
   BUD-12 with cursor pagination, optionally load manifest / drive events (kind 30563) /
   NIP-94 (kind 1063) / NIP-34 git repos.
7. Add virtual `README.txt` and `STATUS.txt`; collect "expiring soon" blobs.
8. Construct `BlossomFS` via `new_rw` (keys + server + RW) or `new_with_cache` (read-only).
9. Spawn the control socket (`{cache_dir}/blossomfs.sock`) and, if `--metrics-port` is set,
   the Prometheus server.
10. Send sd-notify `READY`, call `fuser::mount2` (blocks until unmount).
11. On unmount: send sd-notify `STOPPING`, optionally publish persisted tree (kind 30078).

### Async/Sync Bridge

FUSE callbacks are synchronous; all network I/O is async. Each callback that needs the
network calls `self.runtime_handle.block_on(async { … })`, which enters the tokio thread
pool. fuser spawns its own callback threads, so this is safe — each callback blocks one
thread while awaiting the future. The runtime is created once in `main` and never dropped
during the mount.

## Key Design Decisions

### 1. Lazy-fetch (not mount-time download)

Blobs are **not** downloaded at mount. The tree is built from BUD-12 descriptors (metadata
only); bytes are fetched on the first `read()` of each `FileContent::Remote` node. This
keeps mount time bounded even for accounts with thousands of blobs. Mount time is capped
additionally by `--max-blobs` (default 1000 per server).

### 2. Content-addressed disk cache

Fetched blobs live at `<cache-dir>/objects/<aa>/<bb>/<sha256>` (two-level hex sharding,
65 536 leaf dirs). Paths are deterministic from the hash — no index file. Writes go to a
`.tmp` file then atomically renamed (`NamedTempFile::persist`); a crashed download never
leaves a partial cache entry. SHA-256 is verified **before** the rename — bad data is never
cached.

### 3. No eviction by default

`--max-cache-size` defaults to `0` (unlimited). With the default, the cache grows without
bound — manual `rm -rf cache/objects/` is safe (immutable entries are simply re-fetched).
Setting `--max-cache-size > 0` enables FIFO eviction of the oldest cached blobs when the
cap is exceeded (`object_cache::evict_oldest`).

### 4. RW mode: write-on-flush

Writes are buffered in memory (`WriteStorage::Memory`) and spilled to a `NamedTempFile`
(`WriteStorage::Disk`) when they exceed `--write-buffer-mb` (default 64 MB). Upload fires
on `flush()`: SHA-256 is computed, a BUD-11 kind-24242 auth event is signed with the user's
nsec, and a BUD-02 `PUT /upload` (or multipart, above `--multipart-threshold-mb`) is sent
with `Authorization: Nostr <event>` and `X-SHA-256` headers. On success the tree node
flips from `Static(empty)` to `Remote(sha256, url)`. The `flushed` flag guards against
re-upload (flush can fire multiple times from fork/dup). `release()` is the fallback upload
path if flush never ran.

### 5. Hash-first filesystem layout

Every blob's filename is its SHA-256. Three orthogonal views (`by-sha256/`, `by-type/<mime>/`,
`by-date/YYYY/MM/DD/`) let users browse by hash, MIME type, or upload date. Dedup is trivial:
the same blob from different servers or dates maps to one filename.

### 6. Security posture

- **No redirect following**: `reqwest::redirect::Policy::none()` everywhere — prevents SSRF
  via crafted blob URLs.
- **Path sanitization**: all remote-derived path components pass through `util::path::sanitize`
  (rejects `..`, null bytes, control chars, absolute escapes).
- **Size guard**: blobs over `MAX_BLOB_SIZE` (2 GB) are rejected pre- and post-download.
- **Secret key hygiene**: `nsec` is held in memory for signing only, never logged.

## Testing Protocol (MANDATORY)

When **ANY** change is made to blossomfs, the following MUST be run and pass:

### 1. Unit + integration tests
```bash
cargo test
```
Tests use **wiremock** (`wiremock = "0.6"`) for HTTP mocking — no real Blossom server is
contacted. Test naming convention is scenario-based: `s1_happy_path_…`, `s2_…`, plus `v1_…`
for validation and `c0X_…` for control-socket tests.

### 2. Lint (zero warnings tolerated)
```bash
cargo clippy -- -D warnings
```

### 3. Format check
```bash
cargo fmt --check
```

### reqwest timeout quirk — CRITICAL

Setting `.timeout()` on a `reqwest::Client` **breaks wiremock**: mock-server requests
immediately fail with `TimedOut`. For this reason `BlossomClient` has two constructors:

| Constructor | Timeout | Use for |
|---|---|---|
| `BlossomClient::new(url)` | none | **tests** and one-shot CLI commands (`extend`) |
| `BlossomClient::with_timeout(url, dur)` | configurable | production FUSE operations |

**Never** add a `.timeout()` to `BlossomClient::new()` — every wiremock test will start
failing. The FUSE layer calls `set_http_timeout()` (from `--http-timeout-secs`, default 30s)
and then constructs clients via `with_timeout()` inside `do_upload` / `unlink`.

## Prometheus Metrics

Enabled with `--metrics-port <port>` (or `metrics_port` in config, or `BLOSSOMFS_METRICS_PORT`
env var). When set, `main.rs` calls `metrics::init()` then spawns
`metrics::start_metrics_server(port)` on the tokio runtime.

**The server binds to `127.0.0.1` only** — never `0.0.0.0`. Metrics are intended for local
scraping (node_exporter textfile, sidecar, `curl 127.0.0.1:<port>/metrics`). Do not expose
externally without a reverse proxy adding auth.

Endpoint: `GET http://127.0.0.1:<port>/metrics` → Prometheus text format.

**10 metrics** (`src/metrics.rs`):

| Metric | Type | Description |
|---|---|---|
| `blossomfs_cache_hits_total` | IntCounter | Cache hits (blob served from disk) |
| `blossomfs_cache_misses_total` | IntCounter | Cache misses (blob fetched over network) |
| `blossomfs_uploads_total` | IntCounter | BUD-02 upload count |
| `blossomfs_uploads_bytes_total` | IntCounter | Total uploaded bytes |
| `blossomfs_downloads_total` | IntCounter | Download count (fetch.rs + client.rs get_blob) |
| `blossomfs_downloads_bytes_total` | IntCounter | Total downloaded bytes |
| `blossomfs_errors_total{type="…"}` | IntCounterVec | Errors by category (`hash_mismatch`, `http`, `io`, …) |
| `blossomfs_cache_size_bytes` | IntGauge | Current cache size in bytes |
| `blossomfs_cache_file_count` | IntGauge | Number of files in cache |
| `blossomfs_active_uploads` | IntGauge | Currently in-flight uploads (RAII guard `ActiveUploadGuard`) |

## Known Gotchas

1. **reqwest timeout breaks wiremock** — see Testing Protocol above. `BlossomClient::new()`
   must stay timeout-free.
2. **Cache grows unbounded by default** — `--max-cache-size` defaults to `0` (unlimited).
   There is no LRU sweep or TTL cleanup unless you opt in via `--max-cache-size > 0` (which
   enables FIFO eviction). Manual `rm -rf <cache-dir>/objects/` is always safe.
3. **No HTTP range requests** — the full blob is downloaded even if the application reads a
   small byte range. The FUSE `read()` callback applies offset/size **after** the full
   content is materialized in cache. A `dd if=<file> bs=1 skip=1000000 count=10` of a 1 GB
   blob will download all 1 GB first.
4. **No cross-mount expiry persistence** — `Sunset` header (RFC 8594) expiry data is stored
   in the in-memory tree and lost on unmount. Only known after the first fetch of a blob.
5. **`BlossomConfig` uses `#[serde(deny_unknown_fields)]`** — old config files with removed
   fields will fail to parse. Update config when switching branches.
6. **`rename` and `link` return `ENOSYS` (not `EROFS`) in RW mode** — intentional. They are
   genuinely unimplemented, not blocked by read-only mode.
7. **nsec never logged** — if you add tracing around key material, explicitly redact it.
8. **`block_on` inside FUSE callbacks** — each callback blocks one thread on network I/O.
   This is acceptable because fuser uses a thread pool, but do not introduce long-running
   synchronous work in hot paths.

## Recent Work

- **Prometheus metrics (#24)** — `src/metrics.rs`, `--metrics-port` flag. 10 metrics
  (counters + gauges + one labeled error counter), served by an axum-free raw `TcpListener`
  bound to `127.0.0.1`. `ActiveUploads` uses an RAII guard (`ActiveUploadGuard`) so the
  gauge decrements even on upload failure.
- **BUD-04 mirroring (#21)** — `BlossomClient::mirror_blob(source_url, auth_header)`:
  `PUT /mirror` with JSON body `{"url": "…"}`. The server fetches the blob from the source
  and stores it locally. Used to replicate blobs between Blossom servers without re-uploading
  bytes through the client.

## Build Instructions

```bash
# Debug build
cargo build

# Release build (LTO + strip enabled in [profile.release])
cargo build --release

# Run all tests (wiremock-based, no network needed)
cargo test

# Lint — zero warnings tolerated
cargo clippy -- -D warnings

# Format check
cargo fmt --check

# Format fix
cargo fmt

# Mount (read-only, default)
mkdir -p /mnt/blossom
cargo run --release -- mount \
  --mountpoint /mnt/blossom \
  --npub npub1… \
  --server https://blossom.example.com \
  --relay wss://relay.example.com

# Mount (read-write — requires nsec)
cargo run --release -- mount \
  --mountpoint /mnt/blossom \
  --npub npub1… \
  --server https://blossom.example.com \
  --read-only=false \
  --nsec-file ~/.config/blossomfs/nsec \
  --metrics-port 9100

# Unmount
fusermount -u /mnt/blossom
```

### Configuration sources (precedence high → low)

1. **CLI flags** (`--npub`, `--server`, …) — always win.
2. **Environment variables** prefixed `BLOSSOMFS_` (`BLOSSOMFS_NPUB`, `BLOSSOMFS_TTL_SECS`, …).
   See `ENV_FIELDS` in `src/config.rs` for the allowlist.
3. **TOML config file** (`--config path.toml`), parsed via figment. `deny_unknown_fields` is
   set — unknown keys are rejected.

### Runtime control socket

Listens on `{cache_dir}/blossomfs.sock`. One JSON command per line, one JSON response per
line:

```bash
echo '{"cmd":"status"}'   | socat - UNIX-CONNECT:/tmp/blossomfs/blossomfs.sock
echo '{"cmd":"freeze"}'   | socat - UNIX-CONNECT:/tmp/blossomfs/blossomfs.sock
echo '{"cmd":"unfreeze"}' | socat - UNIX-CONNECT:/tmp/blossomfs/blossomfs.sock
```

`freeze` flips an `AtomicBool` that makes all write FUSE callbacks return `EROFS` at runtime
(without remounting). Useful before backups or server maintenance.
