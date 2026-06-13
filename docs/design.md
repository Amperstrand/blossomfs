# BlossomFS Design Document

Read-only FUSE filesystem that mounts Blossom protocol media as a local directory tree.

---

## Architecture Overview

BlossomFS has four layers, each corresponding to a module in `src/`:

```
┌─────────────────────────────────────────────┐
│                 CLI (cli.rs)                │
│         clap argument parsing, startup     │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│              FUSE Layer (fuse/)             │
│   fs.rs  ·  tree.rs  ·  inode.rs           │
│   fuser::Filesystem trait implementation    │
│   Virtual directory tree, inode allocation  │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│         Blossom Client (blossom/)           │
│   client.rs  ·  descriptor.rs  ·  auth.rs   │
│   Hybrid nostr-blossom + reqwest HTTP       │
│   BUD-01/02/03/11/12 protocol interactions │
└──────────────┬──────────────────────────────┘
               │
┌──────────────▼──────────────────────────────┐
│            Nostr Layer (nostr/)             │
│   keys.rs  ·  discovery.rs  ·  nip94.rs     │
│   legacy_drive.rs                           │
│   Key parsing, relay queries, kind parsing  │
└─────────────────────────────────────────────┘
```

Two cross-cutting concerns sit alongside these layers:

- **Cache Layer** (`cache/`): Content-addressed blob cache on disk (`object_cache.rs`).
- **Utilities** (`util/`): Path sanitization (`path.rs`) and MIME extension inference (`mime.rs`).

---

## Async / Sync Bridge

The core tension in BlossomFS: FUSE callbacks are synchronous, but all network operations (HTTP requests, relay queries) are async.

### Design

1. A tokio multi-threaded runtime is created at startup, before the FUSE mount.
2. The runtime's `Handle` is stored in the `BlossomFS` struct.
3. Each FUSE callback that needs to do network I/O calls `runtime.block_on()`.
4. The FUSE daemon thread blocks on the future, which runs on the tokio runtime's thread pool.

```text
FUSE kernel → fuser callback thread
  → BlossomFS.read(&self, ...)
    → self.runtime.handle().block_on(async {
        self.client.get_blob(sha256).await?;
      })
    → tokio thread pool executes the HTTP request
    → result returned to FUSE callback
```

### Why block_on inside a FUSE callback

- fuser spawns its own threads for FUSE callbacks. These threads are not tokio threads.
- `block_on()` on a tokio `Handle` lets us enter the async runtime from a sync context.
- Each FUSE callback blocks one thread while waiting for the network, which is fine because fuser uses a thread pool and FUSE reads are typically concurrent with bounded parallelism from the kernel.

### Runtime Creation

```text
// In main.rs (simplified)
let runtime = tokio::runtime::Runtime::new()
    .expect("Failed to create tokio runtime");
let handle = runtime.handle().clone();

let blossomfs = BlossomFS::new(config, handle);
fuser::mount2(blossomfs, &mountpoint, options)?;
```

The runtime lives for the duration of the process. It is NOT dropped during the mount.

---

## Filesystem Layout

BlossomFS projects a hash-first view of remote blobs into a navigable directory structure.

```
/
├── README.txt              # Static file describing the mount
├── STATUS.txt               # Live status: server count, blob count, cache size
├── public/
│   └── <npub>/
│       ├── servers/
│       │   └── <host>/
│       │       ├── by-sha256/
│       │       │   └── <sha256>[.<ext>]
│       │       ├── by-type/
│       │       │   └── <mime>/
│       │       │       └── <sha256>[.<ext>]
│       │       └── by-date/
│       │           └── YYYY/
│       │               └── MM/
│       │                   └── DD/
│       │                       └── <sha256>[.<ext>]
│       └── all-servers/
│           └── by-sha256/
│               └── <sha256>[.<ext>]
```

### Layout Rationale

The hash-first design means every blob appears under its SHA-256 as the filename. This eliminates name collisions (SHA-256 is the canonical identifier in Blossom) and makes deduplication trivial: the same blob from different servers or different dates maps to the same filename.

Three orthogonal views give different browsing angles:

| View | Use case |
|---|---|
| `by-sha256/` | Find a blob when you know its hash |
| `by-type/<mime>/` | Browse all blobs of a given type (e.g. `image/png`) |
| `by-date/YYYY/MM/DD/` | Browse blobs by upload date |

The `<ext>` suffix is inferred from the MIME type using `mime_guess`. It is cosmetic only. The SHA-256 hash is the authoritative identifier.

### npub as Top-Level Key

Each pubkey gets its own directory tree. The directory name is the bech32 npub encoding of the pubkey, which is human-readable and unambiguous.

Under each npub:
- `servers/<host>/` shows blobs from a specific Blossom server.
- `all-servers/` is a merged view of blobs across all known servers for that pubkey.

### README.txt and STATUS.txt

`README.txt` is a static file generated at mount time explaining what the mount is.

`STATUS.txt` is dynamically generated on each read, containing:
- Number of servers contacted
- Total blob count
- Cache hit rate
- Cache disk usage

These files are virtual (no corresponding Blossom blob). They are generated in-tree during construction.

---

## Inode Strategy

### Root Inode

Root inode = 1 (`FUSE_ROOT_ID`), as required by the FUSE protocol.

### Sequential Allocation

Inodes are allocated sequentially starting from 2 during tree construction:

```text
1  → / (root)
2  → /README.txt
3  → /STATUS.txt
4  → /public/
5  → /public/<npub1>/
6  → /public/<npub1>/servers/
7  → /public/<npub1>/servers/<host1>/
8  → /public/<npub1>/servers/<host1>/by-sha256/
9  → /public/<npub1>/servers/<host1>/by-sha256/<sha256>.png
...
```

### Bidirectional Map

Two maps provide O(1) lookup in both directions:

- `inode_to_path: HashMap<InodeNo, TreePath>` for FUSE lookups (given an inode, find its path).
- `path_to_inode: HashMap<TreePath, InodeNo>` for path resolution (given a path, find its inode).

### Immutability

The tree is built once at mount time and is immutable during the session. New inodes are never allocated after construction. This avoids lock contention during read operations.

The tree is stored as `Arc<Tree>` (shared, immutable reference). No Mutex needed.

---

## Interior Mutability

BlossomFS separates state into two categories:

### Immutable State: `Arc<Tree>`

The directory tree, inode maps, and blob descriptors. Built once, read-only for the rest of the mount. No synchronization needed beyond `Arc` reference counting.

### Mutable State: `Arc<Mutex<CacheState>>`

The content cache needs interior mutability because:
1. FUSE callbacks take `&self` (fuser 0.17.0 constraint).
2. Blobs are fetched lazily on first read (not pre-fetched at mount time).
3. Multiple FUSE read callbacks may race to fetch the same blob.

```text
struct BlossomFS {
    tree: Arc<Tree>,                    // immutable
    cache: Arc<Mutex<CacheState>>,       // mutable (interior)
    client: BlossomClient,              // internally async
    runtime: tokio::runtime::Handle,     // for block_on bridge
}
```

The Mutex is held only during cache lookup and update, not during the network fetch itself. A fetch-then-update pattern:

1. Lock cache, check if blob is present.
2. If cache hit: unlock, return cached path.
3. If cache miss: unlock, fetch blob to temp file, compute hash.
4. Lock cache, move temp file to final path, mark as cached.
5. Unlock, return path.

This minimizes lock hold time and allows concurrent fetches of different blobs.

---

## Blossom Client: Hybrid Approach

BlossomFS does not rely solely on `nostr-blossom` for HTTP operations. See the Corrections section in `research-validation.md` for the full rationale.

### What comes from nostr-blossom

- `BlobDescriptor` type definition.
- Event kind constants (10063, 24242).
- Auth event construction helpers (kind 24242 signing).

### What uses raw reqwest

- **BUD-12 listing**: `GET /list/<pubkey>?cursor=...&limit=...` with cursor-based pagination. `nostr-blossom` only supports `since`/`until` filters, which are deprecated for pagination.
- **BUD-01 blob download**: `GET /<sha256>` with streaming response. `nostr-blossom` returns `Vec<u8>`, loading the full blob into memory. `reqwest` with the `stream` feature gives us `bytes_stream()` for bounded-memory streaming to disk.

### Client Module Structure

```
blossom/
├── descriptor.rs   # BlobDescriptor type (may use nostr-blossom re-exports)
├── client.rs        # HTTP client: listing (reqwest) + download (reqwest streaming)
└── auth.rs         # Kind 24242 event construction and header encoding
```

---

## Cache Design

The object cache stores fetched blobs on disk, indexed by their SHA-256 hash.

### Layout

```
<cache-dir>/objects/<aa>/<bb>/<sha256>
```

Where `<aa>` is the first two hex characters of the SHA-256 and `<bb>` is the next two. This two-level sharding prevents single-directory inode exhaustion for large caches.

The cache directory is resolved at runtime using the `directories` crate:
- Linux: `~/.cache/blossomfs/`
- macOS: `~/Library/Caches/blossomfs/`

A CLI flag (`--cache-dir`) overrides the default.

### Fetch Flow

```
1. Cache lookup: does <cache-dir>/objects/<aa>/<bb>/<sha256> exist?
   ├── Yes: return the path. (cache hit)
   └── No: proceed to step 2. (cache miss)

2. Stream download:
   GET https://<server>/<sha256> → bytes_stream()
   Write to <cache-dir>/objects/<aa>/<bb>/<sha256>.tmp
   Simultaneously compute SHA-256 over the stream.

3. Hash verification:
   ├── Match: atomic rename .tmp → <sha256>. Cache entry complete.
   └── Mismatch: delete .tmp, return I/O error. Never cache bad data.

4. Subsequent reads: cache hit (step 1).
```

### Atomic Move

The final rename is atomic on the same filesystem (POSIX `rename()` is atomic when source and destination are on the same mount). If the process crashes mid-fetch, the `.tmp` file is left behind and ignored on subsequent lookups.

### Eviction

No automatic eviction in the initial implementation. The user manages cache size manually by deleting the cache directory. A future milestone may add LRU eviction or a size cap.

---

## Nostr Layer

### Key Parsing (`keys.rs`)

Parses bech32-encoded Nostr keys:
- `npub`: public key (used as directory names in the filesystem).
- `nsec`: secret key (used for signing auth events, if auth is needed).

Security constraint: `nsec` values are never logged. Tracing output redacts any private key material.

### Server Discovery (`discovery.rs`)

Queries Nostr relays for kind 10063 events (BUD-03). Extracts ordered `server` tags and populates the list of Blossom servers to contact.

The relay list is configurable via CLI (`--relay` flag, multiple allowed).

### NIP-94 Metadata (`nip94.rs`)

Parses kind 1063 events for file metadata enrichment. Used to populate the `by-type/` view with accurate MIME types and add dimensions, descriptions, and thumbnail URLs to file metadata.

### Legacy Blossom Drive (`legacy_drive.rs`)

Parses kind 30563 events from the deprecated Blossom Drive format. Extracts file entries from `x` tags and folder placeholders from `folder` tags.

All paths from legacy drive events are untrusted input. They pass through the sanitization module before being incorporated into the tree.

---

## Utility Modules

### Path Sanitization (`path.rs`)

All remote metadata (hostnames, MIME types, legacy drive paths) is untrusted. This module enforces:
- No path traversal (`..` components rejected).
- No null bytes.
- No control characters.
- No absolute path escapes.
- Safe hostname components (lowercase, alphanumeric, hyphens, dots).
- Safe MIME type components (lowercase, alphanumeric, slashes, hyphens, plus signs).

### MIME Extension Inference (`mime.rs`)

Maps MIME types to file extensions using the `mime_guess` crate. Extensions are UX-only: `image/png` becomes `.png`, `application/pdf` becomes `.pdf`. Unknown types get no extension.

---

## Security Considerations

### Path Traversal Prevention

All paths constructed from remote data pass through `util::path::sanitize()`. The filesystem tree is built from controlled templates (SHA-256 hashes, known MIME types, date components) with sanitization applied at construction time. A malicious server cannot inject `../../etc/passwd` into the FUSE namespace.

### SHA-256 Verification

Every blob is verified against its expected SHA-256 hash after download. If the hash does not match, the blob is discarded and an error is returned. This prevents:
- Corrupted downloads (network errors, truncated responses).
- Malicious servers serving different content under a known hash.

### Content-Type Trust

MIME types from remote servers are used only for file extensions and directory names, never for programmatic decisions like executing files. The FUSE filesystem is read-only.

### Secret Key Hygiene

Private keys (`nsec`) are held in memory for signing auth events but are never included in log output. The tracing subscriber filters or the auth module explicitly redacts key material before formatting.

### Read-Only Enforcement

All write-related FUSE callbacks (create, write, unlink, rename, etc.) return `EROFS` (Read-only file system). The filesystem cannot be modified through the FUSE interface, even by root.

### Network Security

- HTTP traffic to Blossom servers is over HTTPS where available.
- Auth tokens (kind 24242 events) are scoped to specific actions and optionally to specific servers or blob hashes.
- No credentials are persisted to disk.

---

## Milestone Roadmap

### M0: Project Scaffold (done)

Cargo.toml with all dependencies. Module structure with doc comments. Stub implementations for every module. Builds without warnings.

### M1: CLI and Mount Skeleton

clap-based CLI for `blossomfs mount <mountpoint>`. Tokio runtime creation. fuser::mount2 invocation with a minimal Filesystem impl that returns an empty root directory. Can mount and unmount successfully with `fusermount -u`.

### M2: Tree and Inode Layer

Inode allocator with bidirectional map. Virtual tree construction from a static blob list. FUSE lookup, getattr, readdir implementations. README.txt and STATUS.txt virtual files.

### M3: Blossom Client and Cache

BUD-12 listing via reqwest with cursor pagination. BUD-01 blob download via reqwest streaming. Content-addressed cache with SHA-256 verification and atomic moves. Tree populated from actual server data at mount time.

### M4: Nostr Discovery

Kind 10063 relay queries for server discovery. Relay list from CLI flags. Populate tree from discovered servers automatically. Fallback to manually specified servers.

### M5: Legacy Blossom Drive

Kind 30563 event parsing. Path sanitization for legacy drive entries. Integrated into the tree under a `drives/` namespace or merged into the npub-based tree.

### M6: NIP-94 Metadata Enrichment

Kind 1063 event parsing. Populate `by-type/` views from NIP-94 metadata. Fallback to server-reported MIME types when NIP-94 data is not available.

### M7: Auth and Hardening

Kind 24242 auth event signing. Authenticated listing and retrieval for private blobs. Error handling polish. Rate limit awareness. Comprehensive logging. Integration tests with wiremock.
