# Append-Only Drive: A Namespace Protocol for BlossomFS

Draft specification for an append-only operation log that replaces the single-event Blossom Drive model.

---

## 1. Problem Statement

The original Blossom Drive (github.com/hzrd149/blossom-drive) represented an entire file tree as a single replaceable Nostr event of kind 30563. One event held every file mapping, every folder placeholder, and every piece of metadata. That design worked for toy examples but broke under real usage in four concrete ways.

### Size limits

Nostr relays cap event size at ~100KB. A drive with 200 files at 500 bytes per `x` tag already exceeds that. Adding thumbnails or nested directories makes it worse.

### Race conditions

Replaceable events follow NIP-01 semantics: a newer event with the same kind and author supersedes the older one. Two clients editing the same drive simultaneously each publish a full replacement. One wins, one loses silently.

### No operation history

Each edit replaces the entire event, so there is no audit trail. No way to see who renamed a file or when a blob was unlinked, and no way to undo an accidental delete.

### Poor scaling

Adding one file to a 500-file drive requires re-uploading the entire 500-file event. Bandwidth, latency, and relay load all grow linearly with drive size.

The fix: replace the single replaceable event with an append-only log of individual operations. Each file action is its own signed event. Clients replay the log to derive the tree. No operation ever replaces or deletes a previous one.

---

## 2. Design Principles

**Append-only log.** Every namespace operation is a separate, signed Nostr event. Events are never replaced, never deleted, and never modified after publication. The log grows monotonically.

**Content-addressed blobs.** SHA-256 remains the canonical identifier for blob content, as defined by BUD-01 and BUD-02. A drive event references blobs by hash. The hash is the source of truth for content integrity.

**Names are claims, not properties.** A path like `/documents/report.pdf` is a user's claim about what to call a hash. The same hash can appear under different paths in different drives. Names are metadata layered on top of content-addressed storage.

**Signed operations.** Each event is cryptographically signed by the drive owner's Nostr key, providing tamper-evident history. Multi-author support is possible via delegation.

**Last-writer-wins by default.** When two operations conflict on the same path, the one with the later `created_at` wins. Simple and effective for single-author drives. Multi-author drives can parameterize the conflict strategy.

**Backward compatible.** Old kind 30563 drives continue to work. A migration tool can replay a 30563 event into append-only operations. Clients read both formats during the transition period.

---

## 3. Proposed Event Structure

### Kind number

We propose kind **30570** for append-only drive operations. This falls within the range already associated with Blossom-related event kinds (30563, 30564 for the old drive and its encrypted variant) but does not collide with any existing assignment.

Kind 30570 is a **regular** event, not replaceable. Regular events are never superseded. Relay filters by kind + pubkey + `d` tag retrieve the full operation log.

### Operation types

Each operation event carries an `op` tag identifying the action:

| Operation | `op` tag | Description |
|-----------|----------|-------------|
| link | `link` | Map a path to a blob hash |
| unlink | `unlink` | Remove a path-to-blob mapping |
| rename | `rename` | Move a path to a new path |
| copy | `copy` | Copy a path mapping to a new path |
| mkdir | `mkdir` | Create an empty directory |
| rmdir | `rmdir` | Remove an empty directory |
| metadata | `metadata` | Attach key-value metadata to a path |
| mirror | `mirror` | Pin a blob to a specific server |
| snapshot | `snapshot` | Periodic tree state checkpoint |

### Common tags (all operations)

| Tag | Required | Description |
|-----|----------|-------------|
| `op` | yes | Operation type identifier |
| `d` | yes | Drive identifier (namespaced by author) |
| `path` | yes (except mirror) | Target path within the drive |

The `d` tag follows NIP-51 conventions. Users maintain multiple drives by using different `d` values, scoped to the event author. The `path` tag is always absolute from the drive root using `/` as separator.

### 3.1 link

Map a path to a blob. Creates parent directories implicitly. Required: `op` ("link"), `d`, `path`, `x` (64-char lowercase hex SHA-256). Optional: `size` (decimal string), `m` (MIME type), `uploaded` (Unix timestamp).

```json
{"kind":30570,"content":"","created_at":1700000000,
 "tags":[["op","link"],["d","photos"],["path","/vacation/beach.jpg"],
         ["x","b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553"],
         ["size","477328"],["m","image/jpeg"]]}
```

**Validation:** `x` must be valid 64-char hex. `path` must pass section 6 validation. If `size` is present, must be a non-negative integer. If `m` is present, must be valid MIME type. Later `link` on the same path supersedes earlier; later `unlink` supersedes the `link`.

### 3.2 unlink

Remove a path mapping. The blob itself is not deleted from Blossom servers; only the namespace entry is removed.

```json
{
  "kind": 30570,
  "content": "",
  "created_at": 1700001000,
  "tags": [
    ["op", "unlink"],
    ["d", "photos"],
    ["path", "/vacation/beach.jpg"]
  ]
}
```

| Tag | Required | Description |
|-----|----------|-------------|
| `op` | yes | `"unlink"` |
| `d` | yes | Drive identifier |
| `path` | yes | Absolute path to remove |

**Validation rules:**
- `path` must pass path validation.
- No `x` tag. Unlink is path-only; the blob is irrelevant.

**Conflict semantics:** Later `link` on the same path re-creates it. Unlink on a path that does not exist is a no-op (idempotent).

### 3.3 rename

Move a path to a new location. Works for both files and directories.

```json
{
  "kind": 30570,
  "content": "",
  "created_at": 1700002000,
  "tags": [
    ["op", "rename"],
    ["d", "photos"],
    ["path", "/vacation/beach.jpg"],
    ["newpath", "/vacation/2024-beach.jpg"]
  ]
}
```

Required tags: `op` ("rename"), `d`, `path` (source), `newpath` (destination). Both paths must pass validation. Source must exist; destination must not. If a concurrent event modifies the source or creates at the destination before rename is replayed, the rename is skipped.

### 3.4 copy

Copy a path mapping to a new path. Only copies the namespace entry, not the blob.

```json
{
  "kind": 30570,
  "content": "",
  "created_at": 1700003000,
  "tags": [
    ["op", "copy"],
    ["d", "photos"],
    ["path", "/vacation/beach.jpg"],
    ["newpath", "/wallpapers/beach.jpg"]
  ]
}
```

Required tags: `op` ("copy"), `d`, `path` (source), `newpath` (destination). Source must exist as a file. If the source does not exist at replay time, the copy is a no-op. A later `link` at `newpath` supersedes the copy.

### 3.5 mkdir / 3.6 rmdir

Both take only `op`, `d`, and `path` tags.

```json
{"kind":30570,"content":"","created_at":1700004000,
 "tags":[["op","mkdir"],["d","photos"],["path","/vacation/2024"]]}
```

```json
{"kind":30570,"content":"","created_at":1700005000,
 "tags":[["op","rmdir"],["d","photos"],["path","/vacation/2024"]]}
```

`mkdir` is idempotent and implicitly creates parent directories (so explicit `mkdir` is only needed for empty directories). `rmdir` is skipped if the directory is non-empty at replay time; a subsequent `link` or `mkdir` at that path supersedes the rmdir.

### 3.7 metadata

Attach key-value metadata to a path (descriptions, tags, display names).

```json
{
  "kind": 30570,
  "content": "",
  "created_at": 1700006000,
  "tags": [
    ["op", "metadata"],
    ["d", "photos"],
    ["path", "/vacation/beach.jpg"],
    ["description", "Sunset at Malibu"],
    ["tags", "beach,sunset,vacation"]
  ]
}
```

Required tags: `op` ("metadata"), `d`, `path`. Optional: `description`, `tags` (comma-separated). Clients ignore unknown tags. Later metadata on the same path supersedes earlier metadata. Metadata is lost when the path is unlinked.

### 3.8 mirror

Pin a blob to a specific Blossom server. Recorded in the drive log for tracking; the actual mirroring is a BUD-level PUT operation.

```json
{
  "kind": 30570,
  "content": "",
  "created_at": 1700007000,
  "tags": [
    ["op", "mirror"],
    ["d", "photos"],
    ["x", "b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553"],
    ["server", "cdn.example.com"]
  ]
}
```

Required tags: `op` ("mirror"), `d`, `x`, `server`. Additive: multiple mirrors to different servers coexist. Same blob/server pair is idempotent. Note: `mirror` has no `path` tag.

### 3.9 snapshot

A periodic checkpoint containing the full tree state at a given point. Snapshots allow clients to load drive state without replaying every operation from the beginning.

```json
{
  "kind": 30570,
  "content": "{\"files\":{\"/vacation/beach.jpg\":{\"x\":\"b167...\"},\"size\":477328,\"m\":\"image/jpeg\"},\"dirs\":[\"/vacation\"],\"seq\":42}",
  "created_at": 1700010000,
  "tags": [
    ["op", "snapshot"],
    ["d", "photos"],
    ["seq", "42"]
  ]
}
```

| Tag | Required | Description |
|-----|----------|-------------|
| `op` | yes | `"snapshot"` |
| `d` | yes | Drive identifier |
| `seq` | yes | Sequence number (monotonically increasing) |

The `content` field is a JSON object mapping paths to file attributes (`x`, `size`, `m`, `uploaded`) plus a `dirs` list and `seq` counter.

**Conflict semantics:** Clients use the snapshot with the highest `seq`, then replay operations with `created_at` greater than the snapshot's. Snapshots are a convenience from the drive owner; the full operation log is the ground truth.

---

## 4. Drive Model

### Drive identity

A drive is identified by `(author, d-tag)`. The `d` tag is a short string chosen by the owner (e.g. `"photos"`, `"documents"`). Multiple users can use the same `d` value; they are distinct drives because the pubkey differs.

### Drive state derivation

The current state of a drive is the result of replaying all operation events in `created_at` order:

```
                          Relay query: kind=30570, author=<pk>, d=<drive-id>
                                       |
                                       v
                                   Operation Log
                                       |
                                       v
                              ┌─────────────────┐
     [snapshot] ──────────>  │   Tree Builder   │ <────── [link, mkdir, copy, ...]
                              └────────┬────────┘
                                       |
                                       v
                               Current Tree State
                                       |
                                       v
                              ┌─────────────────┐
                              │   FUSE Mount    │
                              │  /drives/<d>/   │
                              └─────────────────┘
```

1. Fetch the latest snapshot (if any).
2. Load the snapshot as the initial tree.
3. Fetch all operations with `created_at` after the snapshot.
4. Apply each operation in `created_at` order.
5. The result is the current drive state.

### Multiple drives per user

A user publishes operations with different `d` tags. The drive list is discovered by querying all kind 30570 events for a pubkey and extracting distinct `d` values. A separate drive metadata event (kind TBD, replaceable, NIP-51) could advertise drive names and descriptions, but this is out of scope for the core protocol.

---

## 5. Conflict Resolution

### Timestamp ordering

All conflict resolution uses the event `created_at` field. The operation with the higher timestamp wins on the same path. No coordination between clients needed.

### Same-path conflicts

| Scenario | Resolution |
|----------|------------|
| link then link (same path) | Later link wins. Path now points to the newer blob. |
| link then unlink (same path) | Unlink wins. Path is removed. |
| unlink then link (same path) | Link wins. Path is recreated with the new blob. |
| link then rename (source path) | Rename wins. Original path is removed. |
| link then rename (dest overwrites) | Rename is skipped. Destination already exists. |

### Fork detection

Operations on the same path with `created_at` within 60 seconds of each other may indicate a fork. Clients flag these to the user but apply last-writer-wins by default.

### Parameterized policies

The `op` tag convention supports an optional `policy` tag on any operation:

| Policy | Tag | Behavior |
|--------|-----|----------|
| strict (default) | `["policy", "strict"]` | Last-writer-wins. No user interaction. |
| merge | `["policy", "merge"]` | Attempt to combine changes (e.g., metadata from both sides). |
| manual | `["policy", "manual"]` | Flag the conflict for human resolution. |

The `policy` tag is advisory. A client that does not understand it falls back to strict. Policy is per-operation, not per-drive. In practice, most drives will use strict everywhere.

---

## 6. Path Validation Rules

All `path` and `newpath` values must pass these checks before being accepted:

| Rule | Check |
|------|-------|
| No path traversal | Reject any component equal to `".."` |
| Absolute within drive root | Must start with `/`. Reject double slashes, trailing slashes. |
| Normalized separators | Convert all `\` to `/`. Collapse `//` to `/`. Strip trailing `/` unless path is `/`. |
| Max depth | No more than 32 path components (counted by `/` separators). |
| Max component length | Each path component is at most 255 characters. |
| No null bytes | Reject `\0` anywhere in the path. |
| No control characters | Reject ASCII 0x00-0x1F and 0x7F. |
| No leading/trailing whitespace | Strip whitespace from each component. Reject empty components after stripping. |
| Allowed characters | Alphanumeric, `.`, `-`, `_`, `/`, space. Any other character is rejected. |

These rules are conservative. The existing `sanitize_path_component` function in BlossomFS already implements most of them.

---

## 7. Relay Considerations

### Event type

Kind 30570 events are **regular** Nostr events. Regular events are never superseded, which is critical for the append-only property.

### Query pattern

To fetch a drive's operation log:

```
{"kinds": [30570], "authors": ["<pubkey>"], "#d": ["<drive-id>"], "limit": 0}
```

For incremental sync after a known checkpoint:

```
{"kinds": [30570], "authors": ["<pubkey>"], "#d": ["<drive-id>"], "since": <last-created_at>}
```

### Relay storage requirements

Relays MUST persist these events indefinitely. If a relay prunes old events, drive history is lost. Clients should use archive relays committed to long-term storage.

### Garbage collection (optional)

A drive owner may publish a GC marker identifying superseded operations. Clients can discard these locally, but relays should not delete any events.

---

## 8. FUSE Projection

### Mount layout

When BlossomFS mounts a drive, it appears under the existing tree as:

```
/drives/<drive-id>/
    README.txt
    STATUS.txt
    /documents/
        report.pdf
        notes.txt
    /photos/
        beach.jpg
        sunset.png
```

This integrates with the current BlossomFS layout (`/public/<npub>/servers/...`). The `/drives/` namespace sits alongside the existing hash-first views.

### Read-only mount (stage 1)

The initial implementation is read-only. BlossomFS replays the operation log at mount time, builds the tree, and serves it through FUSE. All write callbacks return `EROFS`, matching the current "read-only first" design principle. The tree is immutable during the mount session, consistent with the current `Arc<Tree>` design.

### Large drive handling

For drives with thousands of files, two optimizations help:

**Lazy subtree loading.** The tree starts with stub directories. Children are fetched on `readdir`. The tree builder must know which paths are directories (from `mkdir` and `link` parent inference) without materializing every entry. **Snapshot-first loading.** Start from the latest snapshot, then replay only operations published after it, skipping potentially thousands of operations for long-lived drives.

---

## 9. Migration Path

### Reading old drives

The old Blossom Drive (kind 30563) used a single replaceable event with this tag structure:

```json
{
  "kind": 30563,
  "tags": [
    ["d", "my-drive"],
    ["name", "My Drive"],
    ["x", "<sha256>", "/path/to/file", "12345", "application/pdf"],
    ["folder", "/documents"]
  ]
}
```

### Conversion rules

A migration tool reads a kind 30563 event and emits kind 30570 operations: each `x` tag becomes a `link` operation (with `x`, `path`, `size`, `m`), each `folder` tag becomes a `mkdir`. All converted operations use the original 30563 event's `created_at`. A final `snapshot` operation is emitted with `seq` equal to the total count of converted operations.

### Migration event format

```json
{
  "kind": 30570,
  "content": "Migrated from kind 30563 drive",
  "created_at": 1700000000,
  "tags": [
    ["op", "snapshot"],
    ["d", "my-drive"],
    ["seq", "247"],
    ["migrated-from", "30563"]
  ]
}
```

The `migrated-from` tag is informational. Clients display a notice that the drive was converted from the legacy format.

### Coexistence

During transition, clients check for both kind 30563 and 30570 events for a given `d` value. If 30570 events exist, use the append-only format. If only 30563 exists, fall back to the legacy parser already implemented in `legacy_drive.rs`.

---

## 10. Open Questions

**Kind number allocation.** Kind 30570 is a proposal requiring a formal NIP. The NIP-51 parameterized replaceable range (30000+N) uses replaceable semantics which conflict with append-only needs. A regular kind in the 10000+ Blossom range is another option.

**Replaceable vs. regular events.** This spec uses regular events. An alternative uses parameterized replaceable events with a counter in `d` (e.g. `"photos:42"`), giving each operation its own slot. Tradeoff: clients must track the counter, and querying becomes harder.

**Large drives.** A drive with 10K files means 10K+ operations. Snapshots help but can themselves be large. A Merkle-tree snapshot format (directory nodes as subtrees) could reduce worst-case load.

**Soft links.** Should one path resolve to another path instead of a blob? This enables shared files across directories. Risk: circular references and traversal complexity.

**Encryption.** The old drive had kind 30564 for encrypted drives. NIP-44 and NIP-59 provide primitives. The question is whether to encrypt the operation log (hiding tree structure) or the blobs (already handled by Blossom server auth).

**Multi-author drives.** The current design assumes a single owner. Collaborative drives need delegation: the owner grants signing authority to other pubkeys via NIP-26 or an allowlist in a drive metadata event.

---

## References

| Spec | URL |
|------|-----|
| BUD-01 (Blob Retrieval) | https://github.com/hzrd149/blossom/blob/master/buds/01.md |
| BUD-02 (Blob Upload / Descriptor) | https://github.com/hzrd149/blossom/blob/master/buds/02.md |
| BUD-03 (User Server List) | https://github.com/hzrd149/blossom/blob/master/buds/03.md |
| BUD-11 (Nostr Authorization) | https://github.com/hzrd149/blossom/blob/master/buds/11.md |
| NIP-01 (Basic Protocol) | https://github.com/nostr-protocol/nips/blob/master/01.md |
| NIP-51 (Lists) | https://github.com/nostr-protocol/nips/blob/master/51.md |
| NIP-94 (File Metadata) | https://github.com/nostr-protocol/nips/blob/master/94.md |
| Blossom Drive (legacy) | https://github.com/hzrd149/blossom-drive/blob/master/docs/drive.md |
| BlossomFS Design | docs/design.md |
| BlossomFS Research Validation | docs/research-validation.md |
