# BlossomFS -- FUSE Filesystem for Blossom/Nostr Media

## What is BlossomFS?

BlossomFS is a Linux userspace filesystem that makes Blossom blob storage available as local files. Mount a Blossom server (or a local manifest) and browse, read, upload, and verify blobs through your regular filesystem tools. No special client libraries, no REST calls, no JSON parsing. Just `cat`, `ls`, `find`, `cp`, and everything else that works on files.

Blossom is a media storage layer built on top of Nostr. Blobs are identified by their SHA-256 hash and served over HTTPS. BlossomFS projects that storage into a directory tree so you can interact with it using standard Unix tooling.

## What works now

**M0 -- Research and design.** Validated all Blossom BUD specs, Nostr NIPs, and FUSE library APIs against primary sources. Design documents in `docs/`.

**M1 -- Read-only mount.** Mount a Blossom server or manifest as a local directory. Files appear as they would on the remote server.

**M2 -- BUD-12 listing.** When the server supports the `/list` endpoint (BUD-12), BlossomFS fetches the full blob index at mount time and presents every listed blob as a file.

**M3 -- Lazy fetch, cache, and hash verification.** Blobs are not downloaded until you open them. Once fetched, they are cached locally. Every download is verified against its SHA-256 hash. Corrupted or tampered content is rejected.

**M4 -- Relay-based server discovery (NIP-B7/BUD-03).** Provide `--relay wss://...` and BlossomFS queries kind 10063 events to discover Blossom servers automatically. No `--server` flag needed.

**M5 -- Legacy Blossom Drive (kind 30563).** Read-only exposure of old Blossom Drive folder structures. Drive paths appear under `/drives/<pubkey>/<drive-id>/...`. Path traversal and untrusted input are sanitized.

**M6 -- NIP-94 file metadata (kind 1063).** File metadata events are exposed as JSON sidecars under `/metadata/<sha256>.json`. SHA-256 values from relay events are validated before use.

**M7 -- Append-only drive design.** A design document for a future append-only drive namespace (replacing the deprecated single-replaceable-event model) is in `docs/append-only-drive-spec-draft.md`.

**Read-write mode.** Upload blobs to a Blossom server through the filesystem. Write files to the mount point, and BlossomFS uploads them on close via BUD-02 with BUD-11 authentication. The `X-SHA-256` header is sent on every upload for server-side deduplication. Full POSIX open flag support, including `O_TRUNC` and `O_APPEND`.

**Configurable TTL.** The `--ttl-secs` flag controls the FUSE entry and attribute cache TTL. Defaults to 31536000 (1 year). A long default is safe because Blossom blobs are content-addressed and immutable. Use a lower value for debugging.

**CI.** GitHub Actions integration test suite with 5 scenarios covering Blossom server interaction, Cashu payment flows, and FUSE mount verification.

## What doesn't work yet

- No rename or link operations (returns ENOSYS in RW mode)
- No delete operations via Blossom protocol (BUD-08 delete auth exists but is not wired)
- Write buffers are in-memory and unbounded, designed for files under 100 MB
- No eviction policy for the local cache
- Append-only drive namespace (M7) is designed but not implemented

## Read-Write Mode

BlossomFS supports writing files to a Blossom server through the FUSE mount. When you write a file and close it, BlossomFS computes the SHA-256 hash, signs a BUD-11 auth event (kind 24242) with your private key, and uploads the blob via BUD-02 with the `X-SHA-256` header for server-side deduplication.

### Requirements

RW mode requires three things:

1. `--read-only=false` to enable writes
2. `--nsec-file <path>` (or `--dangerous-nsec-arg` for testing) to provide signing credentials
3. At least one `--server` URL as the upload target

### How it works

- Writes are buffered in memory for the lifetime of the open file handle
- On `close` (or `flush`), the buffer is uploaded to the Blossom server
- `O_TRUNC` truncates the in-memory buffer at open time
- `O_APPEND` sets the write offset to the end of the buffer
- After a successful upload, the blob appears in the filesystem under its SHA-256 hash

### Example: Upload a file

```bash
mkdir -p /tmp/blossomfs-mount

blossomfs mount \
  --mountpoint /tmp/blossomfs-mount \
  --server https://blossom.example.com \
  --nsec-file /path/to/nsec.txt \
  --read-only=false
```

In another terminal, copy a file into the mount:

```bash
cp photo.jpg /tmp/blossomfs-mount/public/<pubkey>/servers/blossom.example.com/by-sha256/
```

When the `cp` command closes the file, BlossomFS uploads it. The file appears under its SHA-256 hash once the upload completes.

### Example: Write a new blob directly

```bash
echo "hello blossom" > /tmp/blossomfs-mount/public/<pubkey>/servers/blossom.example.com/by-sha256/newfile.txt
```

BlossomFS uploads the content on close and the file is renamed to its SHA-256 hash.

### About the nsec file

The `--nsec-file` flag reads a Bech32-encoded `nsec1...` private key from the first line of the given file. The key is used only for signing BUD-11 auth events during uploads. It is never sent to the server.

For testing, `--dangerous-nsec-arg nsec1...` accepts the key directly on the command line. This exposes the key in shell history and process listings. Do not use it outside of a test environment.

## Blossom Server Compatibility

BlossomFS relies on several Blossom server endpoints:

| Endpoint | BUD | Purpose |
|---|---|---|
| `GET /<sha256>` | BUD-01 | Download blob content by hash |
| `PUT /upload` | BUD-02 | Upload blob content (RW mode) |
| `GET /list/<pubkey>` | BUD-12 | Enumerate blobs owned by a pubkey |
| `DELETE /<sha256>` | BUD-08 | Delete blob (not yet implemented) |

### BUD-12 listing: needed but privacy-sensitive

BlossomFS uses BUD-12 listing to discover which blobs exist on a server at mount time. Without it, the filesystem mounts empty and you can only access blobs whose hashes you already know (via a manifest or Nostr events).

However, BUD-12 listing is explicitly marked **unrecommended** in the spec for two reasons:

1. **Privacy.** Anyone who knows a pubkey can enumerate every blob that user has uploaded. There is no access control on the list endpoint in the base spec.
2. **Resource cost.** A server with thousands of blobs must serialize and serve the full index on every request.

The intended discovery path is via Nostr events: NIP-94 file metadata events (kind 1063) and Blossom server lists (kind 10063) let clients find blobs through relays without querying the server directly. BlossomFS supports this through `--relay` mode (M4), but BUD-12 listing is currently the most reliable way to get a complete view of a single server.

### Known server compatibility

| Server | BUD-01 GET | BUD-02 PUT | BUD-12 List | Notes |
|---|---|---|---|---|
| **blossom.psbt.me** | Yes | Yes | Yes | Tested in CI. Cashu payments for blobs over 1 MB. |
| **hzrd149/blossom-server** | Yes | Yes | Disabled by default | Set `list.enabled: true` in server config. |
| **v0l/route96** | Yes | Unknown | No | Lightning payments required for some blobs. |

### Enabling BUD-12 listing on your server

If you run a [hzrd149/blossom-server](https://github.com/hzrd149/blossom-server) instance, listing is disabled by default. Enable it in your server configuration:

```yaml
list:
  enabled: true
```

Be aware of the privacy implications before enabling this on a public server.

### What happens when listing is unavailable

If the server returns 404 or 403 for `/list/<pubkey>`, BlossomFS still mounts successfully. The `STATUS.txt` file in the mount root will indicate that listing failed. You can still:

- Use `--manifest` to provide blob descriptors directly
- Use `--relay` to discover blobs via Nostr events
- Access blobs by hash if you construct paths manually

## Why filenames are not canonical in Blossom

Blobs in Blossom are content-addressed by their SHA-256 hash. A blob with hash `abc123` is the same blob regardless of what anyone calls it. Filenames and directory structures are not part of the blob protocol itself; they are user-specific namespace claims layered on top.

This means there is no single "correct" filename for a blob. Different users may assign different names to the same content. BlossomFS uses the hash as the canonical identifier and derives filenames from metadata (MIME type, extension hints) when available. The hash is always the ground truth.

## How the old Blossom Drive worked and why this project is different

The original Blossom Drive design used a single NIP-89 kind 30563 event to represent an entire drive namespace. One event, one replaceable document describing every file and folder. This works fine for small drives but becomes problematic at scale: every change replaces the entire event, clients must parse the full structure on every update, and there is no way to do incremental sync.

This project takes a different approach. BlossomFS uses a hash-first view where each blob is addressed by its SHA-256 hash directly. There is no single drive-defining event to replace or conflict over. An append-only drive design, where namespace metadata accumulates over time rather than being replaced in place, is planned as future work. This avoids the single-event bottleneck and makes incremental updates natural.

## Filesystem Layout

BlossomFS organizes blobs under a `/public/` hierarchy rooted at the mount point. Two virtual files (`README.txt` and `STATUS.txt`) live at the top level. Blob content is projected under `public/` organized by source and server.

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

## CLI Reference

```
blossomfs mount [OPTIONS]

OPTIONS:
  --mountpoint <PATH>        FUSE mount point (required)
  --npub <NPUB>              Bech32 public key (npub1...)
  --pubkey <HEX>             Hex public key (64 hex chars)
  --server <URL>             Blossom server URL (repeatable)
  --manifest <PATH>          Path to manifest JSON file
  --cache-dir <PATH>         Cache directory (default: /tmp/blossomfs)
  --read-only <BOOL>         Mount read-only or RW (default: true)
  --nsec-file <PATH>         File containing nsec for authenticated operations
  --dangerous-nsec-arg <NSEC> Raw nsec on CLI (testing only; leaks to shell history)
  --relay <URL>              Nostr relay URL for server discovery (repeatable)
  --ttl-secs <SECONDS>       FUSE cache TTL (default: 31536000 = 1 year)
```

At least one of `--npub`, `--pubkey`, `--server`, or `--manifest` must be provided. In RW mode, `--nsec-file` (or `--dangerous-nsec-arg`) and at least one `--server` are required.

## Install dependencies

BlossomFS targets Ubuntu 24.04 with FUSE3. Install the required system packages:

```bash
sudo apt-get update
sudo apt-get install -y fuse3 libfuse3-dev pkg-config build-essential git curl jq
```

## Build

```bash
cargo build --release
```

The binary will be at `./target/release/blossomfs`.

## Demo: Manifest mode (read-only)

Manifest mode lets you mount blobs from a local JSON file without connecting to a server. An example manifest with four sample descriptors is included at `examples/manifest.json`.

```bash
mkdir -p /tmp/blossomfs-mount
./target/release/blossomfs mount --manifest examples/manifest.json --mountpoint /tmp/blossomfs-mount
```

In another terminal, browse the mounted filesystem:

```bash
find /tmp/blossomfs-mount -maxdepth 5 -type f -print
cat /tmp/blossomfs-mount/README.txt
```

## Demo: Real Blossom server (read-only)

To mount a live Blossom server, provide an `npub` key and server URL:

```bash
./target/release/blossomfs mount \
  --npub npub1... \
  --server https://blossom.example.com \
  --mountpoint /tmp/blossomfs-mount
```

Replace `npub1...` with your actual public key and `https://blossom.example.com` with your server's address. The `--read-only` flag defaults to `true`, so the mount is read-only without specifying it.

## Demo: Read-write mode

Upload blobs to a Blossom server through the filesystem:

```bash
./target/release/blossomfs mount \
  --npub npub1... \
  --server https://blossom.example.com \
  --nsec-file /path/to/nsec.txt \
  --read-only=false \
  --mountpoint /tmp/blossomfs-mount
```

Then write files into the mount. They upload on close:

```bash
cp photo.jpg /tmp/blossomfs-mount/public/npub1.../servers/blossom.example.com/by-sha256/
```

Check `STATUS.txt` for upload results.

## Unmount

When you are done, unmount the filesystem:

```bash
fusermount3 -u /tmp/blossomfs-mount
```

If `fusermount3` fails (for example, the process crashed without unmounting cleanly), fall back to:

```bash
sudo umount /tmp/blossomfs-mount
```

## Troubleshooting

**`/dev/fuse` is missing.** The FUSE kernel module is not loaded. Load it with:

```bash
sudo modprobe fuse
```

**Permission denied when mounting.** Your user is not in the `fuse` group. Add yourself and relog:

```bash
sudo usermod -aG fuse $USER
```

Then log out and back in (or start a new shell session) for the group change to take effect.

**Server does not support `/list`.** BlossomFS will still mount, but it will not know about any blobs in advance. A `STATUS.txt` file in the mount directory will explain the situation. You can still access specific blobs by hash if you know them.

**Auth required.** If the server requires authentication and you have not provided credentials, BlossomFS will mount with an error status. Check `STATUS.txt` in the mount point for details.

**Empty mount.** If no blob descriptors are found (empty manifest, server returned no listings), the mount still succeeds but the directory will contain only `STATUS.txt`. This is not an error; it means there is simply nothing to show.

**Hash verification failed.** A downloaded blob did not match its expected SHA-256 hash. This usually means a corrupted download or a man-in-the-middle attack. Clear the local cache directory and try again. If the error persists, the server may be serving incorrect content.

**Upload failed.** Check `STATUS.txt` for error details. Common causes: the server does not support BUD-02 upload, the nsec key does not match the npub, or the server requires payment for blobs over its free tier limit.

## License

MIT. See [LICENSE](LICENSE).
