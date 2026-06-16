# BlossomFS

BlossomFS is a Linux FUSE filesystem for [Blossom](https://github.com/hzrd149/blossom), the media storage layer built on Nostr. Mount a Blossom server as a local directory and browse, read, upload, and verify blobs with standard Unix tools. No client libraries, no REST calls. Just `cat`, `ls`, `find`, `cp`.

## Quickstart

```bash
# 1. Install dependencies (Ubuntu 24.04)
sudo apt-get update
sudo apt-get install -y fuse3 libfuse3-dev pkg-config build-essential git curl jq

# 2. Build
cargo build --release

# 3. Mount a server (read-only)
mkdir -p /tmp/blossomfs-mount
./target/release/blossomfs mount \
  --npub npub1... \
  --server https://blossom.psbt.me \
  --mountpoint /tmp/blossomfs-mount
```

In another terminal:

```bash
find /tmp/blossomfs-mount -maxdepth 5 -type f -print
cat /tmp/blossomfs-mount/README.txt
```

Unmount when done:

```bash
fusermount3 -u /tmp/blossomfs-mount
```

## Features

- **Read-only mount** of Blossom servers or local manifest files
- **BUD-12 listing** to discover all blobs owned by a pubkey at mount time
- **Lazy fetch with SHA-256 verification** -- blobs download on first read, are cached locally, and every byte is verified against its hash
- **Relay-based server discovery** (NIP-B7/BUD-03) via `--relay`
- **Read-write mode** -- write files into the mount, BlossomFS uploads on close via BUD-02 with BUD-11 auth
- **BUD-08 delete** -- remove blobs via `rm` in RW mode
- **Legacy Blossom Drive** (kind 30563) -- browse old drive structures under `/drives/`
- **NIP-94 file metadata** (kind 1063) -- metadata exposed as JSON sidecars under `/metadata/`
- **NIP-34 Git Browser** -- browse git repos published via NIP-34 as filesystem directories
- **Tollgate releases** -- software releases from NIP-94 events under `/tollgate/`
- **Configurable cache TTL** and free-tier parameters

## How It Works

BlossomFS builds an in-memory virtual directory tree at mount time, then serves it over FUSE3. Blobs are content-addressed by SHA-256 hash. When a tree node is a remote blob, the first `read()` triggers an HTTP fetch, SHA-256 verification, and a write to a sharded on-disk cache. Subsequent reads come from cache with no network traffic.

The tree is populated from four sources, depending on the flags you provide:

1. **Manifest** (`--manifest`) -- a local JSON file listing blob descriptors
2. **BUD-12 server listing** (`--server`) -- the server's `/list/<pubkey>` endpoint
3. **Nostr relays** (`--relay`) -- kind 10063 server lists, kind 30563 drives, kind 1063 metadata
4. **NIP-34 relays** (`--nip34-relay`) -- kind 30617 git repos

See [docs/CACHING.md](docs/CACHING.md) for the full caching architecture.

## Usage

### Manifest mode (read-only demo)

Mount blobs from a local JSON file without connecting to any server. An example manifest is at `examples/manifest.json`.

```bash
./target/release/blossomfs mount \
  --manifest examples/manifest.json \
  --mountpoint /tmp/blossomfs-mount
```

### Server mode (read-only)

Mount a live Blossom server. Provide an `npub` key and server URL:

```bash
./target/release/blossomfs mount \
  --npub npub1... \
  --server https://blossom.psbt.me \
  --mountpoint /tmp/blossomfs-mount
```

The `--read-only` flag defaults to `true`, so the mount is read-only by default.

### Read-write mode

Upload blobs to a Blossom server through the filesystem. Writes are buffered in memory and uploaded on file close via BUD-02 with BUD-11 authentication.

```bash
./target/release/blossomfs mount \
  --npub npub1... \
  --server https://blossom.psbt.me \
  --nsec-file /path/to/nsec.txt \
  --read-only=false \
  --mountpoint /tmp/blossomfs-mount
```

Write files into the mount:

```bash
cp photo.jpg /tmp/blossomfs-mount/public/npub1.../servers/blossom.psbt.me/by-sha256/
```

BlossomFS uploads the content on close and the file appears under its SHA-256 hash. Deleting files triggers a BUD-08 authenticated delete request to the server.

The `--nsec-file` flag reads a Bech32-encoded `nsec1...` private key from the first line of the file. The key is used only for signing auth events during uploads and deletes. It is never sent to the server. For testing only, `--dangerous-nsec-arg nsec1...` accepts the key on the CLI, but this leaks it to shell history and process listings.

### NIP-34 Git Browser

Browse git repositories published via NIP-34 (kind 30617) as filesystem directories. Repositories appear under `/git/<pubkey>/`. Each repo exposes its file tree, and with `--nip34-clone`, a shallow `git clone` is triggered on first access for the full working tree.

```bash
./target/release/blossomfs mount \
  --nip34-relay wss://relay.example.com \
  --nip34-pubkey npub1... \
  --nip34-clone \
  --mountpoint /tmp/blossomfs-mount
```

The `--nip34-pubkey` accepts either a hex pubkey or an `npub1...` Bech32 string.

### Tollgate releases

When NIP-94 metadata events contain tollgate release tags, BlossomFS builds a release directory tree under `/tollgate/`. This is populated automatically when using `--relay` with a pubkey that has published release events. No additional flags are needed.

## Configuration File

BlossomFS supports a TOML configuration file via `--config <path>`. All CLI flags (except `--mountpoint` and `--dangerous-nsec-arg`) can be set in the config file. CLI arguments override config values.

```toml
# ~/.config/blossomfs.toml
# All fields are optional. CLI args take precedence.

npub = "npub1..."
server = ["https://blossom.psbt.me"]
relay = ["wss://relay.example.com"]
nsec_file = "/home/user/.config/blossomfs/nsec.txt"
cache_dir = "/var/cache/blossomfs"
read_only = true
ttl_secs = 31536000
max_write_mb = 100
free_period_days = 30
max_free_size_mb = 1
max_cache_size = 0
nip34_relay = ["wss://relay.example.com"]
nip34_pubkey = "npub1..."
nip34_clone = false
```

All config fields are optional. Precedence: **CLI args > environment variables > config file > defaults**.

### Environment Variables

Any config field can also be set via environment variables with the `BLOSSOMFS_` prefix. Environment variables override the config file but are overridden by CLI arguments.

```bash
BLOSSOMFS_CACHE_DIR=/var/cache/blossomfs \
BLOSSOMFS_TTL_SECS=3600 \
BLOSSOMFS_NPUB=npub1... \
./target/release/blossomfs mount --mountpoint /mnt/blossom
```

Then mount with just the mountpoint and config:

```bash
./target/release/blossomfs mount \
  --config ~/.config/blossomfs.toml \
  --mountpoint /tmp/blossomfs-mount
```

## CLI Reference

```
blossomfs mount [OPTIONS]

OPTIONS:
      --mountpoint <PATH>           FUSE mount point (required)
      --npub <NPUB>                 Bech32 public key (npub1...)
      --pubkey <HEX>                Hex public key (64 hex chars)
      --server <URL>                Blossom server URL (repeatable)
      --manifest <PATH>             Path to manifest JSON file
      --cache-dir <PATH>            Cache directory (default: /tmp/blossomfs)
      --read-only <BOOL>            Mount read-only or RW (default: true)
      --nsec-file <PATH>            File containing nsec for authenticated operations
      --dangerous-nsec-arg <NSEC>   Raw nsec on CLI (testing only; leaks to shell history)
      --relay <URL>                 Nostr relay URL for server discovery (repeatable)
      --nip34-relay <URL>           NIP-34 relay for git repo browsing (repeatable)
      --nip34-pubkey <PUBKEY>       NIP-34 pubkey whose repos to browse (hex or npub)
      --nip34-clone                 Enable lazy git clone for NIP-34 repos (default: false)
      --ttl-secs <SECONDS>          FUSE cache TTL in seconds (default: 31536000 = 1 year)
      --max-write-mb <MB>           Max write buffer size per file in MB (default: 100)
      --free-period-days <DAYS>     Free storage period for small files in days (default: 30)
--max-free-size-mb <MB>       Max file size eligible for free storage in MB (default: 1)
--max-cache-size <MB>         Max cache size in MB, 0 = unlimited (default: 0)
                              When exceeded, oldest blobs are evicted (FIFO).
--config <PATH>               Path to TOML configuration file
      --daemon                      Run in background (fork to daemon after mount)
```

At least one of `--npub`, `--pubkey`, `--server`, or `--manifest` must be provided. In RW mode, `--nsec-file` (or `--dangerous-nsec-arg`) and at least one `--server` are required.

## Filesystem Layout

BlossomFS organizes blobs under a `/public/` hierarchy rooted at the mount point. Two virtual files (`README.txt` and `STATUS.txt`) live at the top level.

### Manifest mode (`--manifest`)

```
/
  README.txt
  STATUS.txt
  public/
    local/
      servers/
        manifest/
          by-sha256/
            <sha256>[.<ext>]
          by-type/
            <mime_sanitized>/
              <sha256>[.<ext>]
          by-date/
            YYYY/MM/DD/
              <sha256>[.<ext>]
```

### Server mode (`--server`)

Each server's blobs appear under `public/<pubkey>/servers/<hostname>/`. An aggregated, deduplicated view of all servers is under `public/<pubkey>/all-servers/by-sha256/`.

```
/
  README.txt
  STATUS.txt
  public/
    <pubkey>/                          ("all" if no pubkey)
      servers/
        <hostname>/
          by-sha256/
            <sha256>[.<ext>]
          by-type/
            <mime_sanitized>/
              <sha256>[.<ext>]
          by-date/
            YYYY/MM/DD/
              <sha256>[.<ext>]
      all-servers/
        by-sha256/                     (deduplicated across all servers)
          <sha256>[.<ext>]
```

When both `--manifest` and `--server` are provided, both subtrees coexist under `/public/`.

### Additional virtual directories

- `/drives/<pubkey>/<drive-id>/...` -- Legacy Blossom Drive (kind 30563)
- `/metadata/<sha256>.json` -- NIP-94 file metadata (kind 1063)
- `/nip94/<pubkey>/<filename>` -- NIP-94 files with original filenames
- `/tollgate/` -- Tollgate software releases
- `/git/<pubkey>/<repo-id>/...` -- NIP-34 git repos

## Caching

BlossomFS uses a lazy-fetch, content-addressed disk cache. Blobs are not downloaded at mount time. They are fetched on first `read()`, verified via SHA-256, and written to a sharded on-disk cache under `--cache-dir`. Subsequent reads serve from disk.

For the full caching architecture, expiry tracking, and write/delete paths, see [docs/CACHING.md](docs/CACHING.md).

## systemd / fstab

BlossomFS can be mounted at boot via fstab with the `fuse.blossomfs` wrapper. Example scripts are available in `contrib/`.

```
blossomfs  /mnt/blossom  fuse.blossomfs  _netdev,nofail,config=/etc/blossomfs.toml  0  0
```

For daemonized mounting with systemd, a template unit file is in `contrib/blossomfs@.service`. The `--daemon` flag forks BlossomFS to the background after mount for non-systemd use cases.

## Docker

A multi-stage Dockerfile produces a ~150MB image based on Ubuntu 24.04:

```bash
docker build -t blossomfs .
```

Run with FUSE access:

```bash
docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  -v /mnt/blossom:/mnt/blossom \
  -v ~/.config/blossomfs.toml:/etc/blossomfs.toml:ro \
  blossomfs \
  mount --mountpoint /mnt/blossom --config /etc/blossomfs.toml
```

The `--device /dev/fuse` and `--cap-add SYS_ADMIN` flags are required for FUSE mounts inside containers.

## Server Compatibility

BlossomFS uses these Blossom server endpoints:

| Endpoint | BUD | Purpose |
|---|---|---|
| `GET /<sha256>` | BUD-01 | Download blob content by hash |
| `PUT /upload` | BUD-02 | Upload blob content (RW mode) |
| `GET /list/<pubkey>` | BUD-12 | Enumerate blobs owned by a pubkey |
| `DELETE /<sha256>` | BUD-08 | Delete blob (RW mode) |

### Known server compatibility

| Server | BUD-01 GET | BUD-02 PUT | BUD-08 DELETE | BUD-12 List | Notes |
|---|---|---|---|---|---|
| **blossom.psbt.me** | Yes | Yes | Yes | Yes | Tested in CI. Cashu payments for blobs over 1 MB. |
| **hzrd149/blossom-server** | Yes | Yes | Yes | Disabled by default | Set `list.enabled: true` in server config. |
| **v0l/route96** | Yes | Unknown | Unknown | No | Lightning payments required for some blobs. |

### Enabling BUD-12 listing on your server

If you run a [hzrd149/blossom-server](https://github.com/hzrd149/blossom-server) instance, listing is disabled by default:

```yaml
list:
  enabled: true
```

Be aware of the privacy implications. BUD-12 lets anyone enumerate every blob owned by a pubkey with no access control.

### What happens when listing is unavailable

If the server returns 404 or 403 for `/list/<pubkey>`, BlossomFS still mounts successfully. The `STATUS.txt` file in the mount root will indicate that listing failed. You can still use `--manifest`, `--relay`, or construct paths to blobs by hash manually.

## Troubleshooting

**`/dev/fuse` is missing.** Load the FUSE kernel module:

```bash
sudo modprobe fuse
```

**Permission denied when mounting.** Add yourself to the `fuse` group and relog:

```bash
sudo usermod -aG fuse $USER
```

**Server does not support `/list`.** The mount still works but starts empty. Use `--manifest` or `--relay` to populate it.

**Auth required.** Check `STATUS.txt` in the mount point. You may need `--nsec-file` for RW operations.

**Empty mount.** Not an error. It means no blob descriptors were found. Check `STATUS.txt` for details.

**Hash verification failed.** A downloaded blob did not match its SHA-256 hash. Clear the cache and retry:

```bash
rm -rf /tmp/blossomfs/objects/*
```

If the error persists, the server may be serving incorrect content.

**Upload failed.** Check `STATUS.txt`. Common causes: server does not support BUD-02, the nsec key does not match the npub, or the blob exceeds the server's free tier limit.

**Config file not found.** Verify the path passed to `--config`. BlossomFS exits with an error if the file does not exist or contains invalid TOML.

## Limitations

- No rename or link operations (returns ENOSYS in RW mode)
- Write buffers are in-memory, designed for files under 100 MB (adjustable via `--max-write-mb`)
- Cache eviction is FIFO by file mtime; use `--max-cache-size` to enable automatic cleanup (0 = unlimited)
- Append-only drive namespace is designed but not implemented
- No range requests during fetch; the full blob is downloaded regardless of read offset

## Why filenames are not canonical in Blossom

Blobs in Blossom are content-addressed by their SHA-256 hash. Filenames and directory structures are not part of the blob protocol. They are user-specific namespace claims layered on top. Different users may assign different names to the same content. BlossomFS uses the hash as the canonical identifier and derives filenames from metadata (MIME type, extension hints) when available.

## How the old Blossom Drive worked and why this project is different

The original Blossom Drive used a single NIP-89 kind 30563 event to represent an entire drive namespace. One event, one replaceable document describing every file and folder. This works for small drives but becomes problematic at scale: every change replaces the entire event, clients must parse the full structure on every update, and incremental sync is impossible.

BlossomFS uses a hash-first view where each blob is addressed by its SHA-256 hash directly. There is no single drive-defining event to replace or conflict over. An append-only drive design, where namespace metadata accumulates over time, is planned as future work.

## License

MIT. See [LICENSE](LICENSE).
