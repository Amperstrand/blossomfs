# BlossomFS Caching Architecture

## Overview

BlossomFS uses a **lazy-fetch with content-addressed disk cache**. Blobs are
not downloaded at mount time — they are fetched on first `read()`, verified
via SHA-256, and written to a sharded on-disk cache. Subsequent reads of the
same blob serve from disk with zero network overhead.

## Cache Directory Structure

```
<cache-base>/
├── objects/
│   └── ab/                      # first 2 hex chars of sha256
│       └── cd/                  # next 2 hex chars
│           └── abcd1234...64    # full 64-char lowercase hex (blob content)
│
└── .tmp/                        # temporary files during download
    └── abcd1234_1700000000_12345   # <prefix>_<unix_ts>_<pid>
```

Two-level sharding (256 x 256 = 65,536 leaf directories) prevents any single
directory from accumulating too many entries. Paths are deterministic from the
SHA-256 hash — no index file needed.

**Source**: `src/cache/object_cache.rs`

| Function | Purpose |
|----------|---------|
| `cache_path(base, sha256)` | Computes `<base>/objects/aa/bb/<sha256>` |
| `cache_exists(base, sha256)` | Checks if cached blob exists on disk |
| `read_cached(base, sha256)` | Reads full bytes from cache |
| `ensure_cache_dir(base, sha256)` | Creates parent directory (`mkdir -p`) |
| `temp_path(base, sha256)` | Generates unique temp file path |

**Validation**: SHA-256 strings are validated by `sanitize_sha256()` in
`src/util/path.rs` — must be exactly 64 hex characters, normalized to
lowercase. Non-hex characters, wrong length, and path traversal attempts are
rejected.

## Fetch Lifecycle

When the FUSE kernel module sends a `read()` request for a file backed by
`FileContent::Remote`, the following sequence executes:

```
FUSE read(ino, offset, size)
  |
  v
fs.rs::read_content()
  |
  v
fetch_and_cache(url, sha256, cache_base)     [src/cache/fetch.rs]
  |
  |-- cache_exists?  --YES-->  read_cached() --> return (bytes, None)
  |                               (no HTTP request)
  |
  |-- NO --+
           v
     HTTP GET (redirects disabled)
           |
           v
     Size guard: reject if > 2GB (MAX_BLOB_SIZE)
           |
           v
     Parse "Sunset" header (RFC 8594) --> Option<u64> expiry
           |
           v
     Download body bytes
           |
           v
     SHA-256 verify: computed == expected?  --NO--> HashMismatch error
           |                                      (bad data never cached)
           |  YES
           v
     ensure_cache_dir() --> write temp file --> atomic rename
           |
           v
     return (bytes, sunset_ts)
  |
  v
if sunset_ts: tree.set_expires(ino, sunset_ts)
  |
  v
return bytes[offset..end] to kernel
```

### Security Measures

- **No redirect following**: `reqwest::redirect::Policy::none()` — prevents
  SSRF via crafted blob URLs
- **SHA-256 verification**: Downloaded content is hashed and compared to the
  expected hash before caching. Mismatched data is never written to disk.
- **Size limit**: Blobs exceeding 2GB (`MAX_BLOB_SIZE`) are rejected
- **Atomic writes**: Data is written to a temp file first, then atomically
  renamed. A crashed download never leaves a partial cache entry.

## Expiry Tracking

### Source 1: HTTP Sunset Header (runtime)

When a blob is fetched, the server MAY include a `Sunset` header (RFC 8594)
indicating when the blob will become unavailable:

```
HTTP/1.1 200 OK
Sunset: Wed, 11 Nov 2026 11:11:11 GMT
```

The date is parsed via `httpdate::parse_http_date()` and converted to a Unix
timestamp. This is stored in the tree node via `tree.set_expires(ino, ts)`.

**Limitation**: Expiry from the Sunset header is only known after the first
fetch. On next mount, the tree is rebuilt from scratch and this data is lost.

### Source 2: BlobDescriptor.expiration (mount time)

The `BlobDescriptor` struct includes an optional `expiration: Option<u64>`
field with `#[serde(default)]`. If a Blossom server includes this field in
its BUD-12 list response, it is threaded through to `FileContent::Remote.expires`
at tree-build time.

**Note**: The BUD-12 spec does NOT standardize an `expiration` field. Servers
MAY include additional fields. This is a forward-compatible extension — if no
server sends it, `expiration` defaults to `None` and has no effect.

### Source 3: Free-period fallback (small files)

For files at or below `max_free_size_bytes` (default: 1MB) with no explicit
expiry, a fallback is computed as `uploaded + free_period_secs` (default: 30
days). This is exposed via the `user.blossom.expiry` extended attribute
(`getxattr`) but does NOT populate `expires` in the tree.

**Source**: `fs.rs::compute_effective_expiry()` — checks `expires` first, then
falls back to the free-period calculation.

### STATUS.txt "Expiring Soon"

At mount time, `main.rs` calls `tree.collect_expiring_blobs(now, 7 * 86400)`
which walks all tree nodes and collects `FileContent::Remote` files where
`expires` is set and falls within the next 7 days. The result populates
`MountInfo.expiring_soon`, rendered by `generate_status()` in `vfiles.rs`.

In practice, this list will be empty unless:
- A Blossom server includes `expiration` in BUD-12 descriptors, OR
- Blobs have been fetched during this mount session and the server returned
  a `Sunset` header

## Write Path (RW Mode)

In read-write mode (`--read-only=false`), files created via FUSE are buffered
in memory and uploaded on flush:

```
FUSE create() --> WriteBuffer allocated in write_state HashMap
FUSE write()  --> data appended to WriteBuffer (max 100MB)
FUSE flush()  --> SHA-256 computed, upload auth signed (BUD-11 kind 24242),
                  PUT /upload with Nostr auth header, descriptor parsed,
                  tree node updated from Static to Remote
```

## Delete Path (RW Mode)

```
FUSE unlink() --> lookup node, extract sha256 from Remote
              --> sign delete auth (BUD-11, t=delete)
              --> DELETE /<sha256> with Nostr auth header
              --> remove cache file if present
              --> remove node from tree
```

## Configuration

| CLI Flag | Default | Description |
|----------|---------|-------------|
| `--cache-dir` | `/tmp/blossomfs` | Cache directory path |
| `--read-only` | `true` | Set to `false` for RW mode |
| `--max-write-mb` | `100` | Max in-memory write buffer per file |
| `--free-period-days` | `30` | Free storage period for small files |
| `--max-free-size-mb` | `1` | Max file size eligible for free storage |

## Known Limitations

1. **No cache eviction**: The cache grows unbounded. There is no LRU sweep,
   size cap, or TTL-based cleanup. Manual cleanup (`rm -rf cache/objects/`)
   is safe since cache entries are immutable and will simply be re-fetched.

2. **No cross-mount expiry persistence**: Expiry data from the `Sunset` header
   is stored in the in-memory tree and lost on unmount. A future enhancement
   could persist expiry alongside the cache file (sidecar `.meta` file or
   filesystem xattr).

3. **No background refresh**: Blobs approaching expiry are not automatically
   re-fetched or refreshed. The `Sunset` header is only checked on the initial
   fetch.

4. **No range requests**: Full blob is downloaded even if the application
   requests a small byte range. The FUSE `read()` callback applies offset/size
   after the full content is in cache.
