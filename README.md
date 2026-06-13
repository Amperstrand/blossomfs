# BlossomFS -- Read-Only FUSE Filesystem for Blossom/Nostr Media

## What is BlossomFS?

BlossomFS is a Linux userspace filesystem that makes Blossom blob storage available as local files. Mount a Blossom server (or a local manifest) and browse, read, and verify blobs through your regular filesystem tools. No special client libraries, no REST calls, no JSON parsing. Just `cat`, `ls`, `find`, and everything else that works on files.

Blossom is a media storage layer built on top of Nostr. Blobs are identified by their SHA-256 hash and served over HTTPS. BlossomFS projects that storage into a directory tree so you can interact with it using standard Unix tooling.

## What works now

**M0 -- Read-only mount.** Mount a Blossom server or manifest as a local directory. The filesystem rejects all write operations. Files appear as they would on the remote server.

**M1 -- Manifest mode.** Point BlossomFS at a JSON file of BUD-02 blob descriptors instead of a live server. Useful for testing, offline work, and scripted workflows. See `examples/manifest.json` for the expected format.

**M2 -- BUD-12 listing.** When the server supports the `/list` endpoint (BUD-12), BlossomFS fetches the full blob index at mount time and presents every listed blob as a file.

**M3 -- Lazy fetch, cache, and hash verification.** Blobs are not downloaded until you open them. Once fetched, they are cached locally. Every download is verified against its SHA-256 hash. Corrupted or tampered content is rejected.

## What doesn't work yet

- No writes, uploads, or deletes
- No rename or move operations
- No Blossom Drive namespace support
- No Nostr event signing or publishing

## Why read-only first?

Read-only is the safe starting point. There is no risk of accidental data loss, no conflict resolution to worry about, and the implementation surface is smaller. The goal is to get the core projection right, the hash-first view of blob storage working correctly, before adding mutation. Once reads are solid and well-tested, writes become a natural extension.

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

## Demo: Manifest mode

Manifest mode lets you mount blobs from a local JSON file without connecting to a server. An example manifest with four sample descriptors is included at `examples/manifest.json`.

```bash
mkdir -p /tmp/blossomfs-mount
./target/release/blossomfs mount --manifest examples/manifest.json --mountpoint /tmp/blossomfs-mount --read-only
```

In another terminal, browse the mounted filesystem:

```bash
find /tmp/blossomfs-mount -maxdepth 5 -type f -print
cat /tmp/blossomfs-mount/README.txt
```

## Demo: Real Blossom server

To mount a live Blossom server, provide an `npub` key and server URL:

```bash
./target/release/blossomfs mount --npub npub1... --server https://blossom.example.com --mountpoint /tmp/blossomfs-mount --read-only
```

Replace `npub1...` with your actual public key and `https://blossom.example.com` with your server's address. The server must support Blossom's blob endpoints for this to work.

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

## License

MIT. See [LICENSE](LICENSE).
