# Directory Persistence — Research & Planning

## Problem
Directories in BlossomFS are in-memory only and lost on remount. Users must re-create directory structures every time the filesystem is mounted.

## Prior Art Research

### 1. Hashtree (mmalmi/hashtree) — RECOMMENDED
- **Repo**: https://github.com/mmalmi/hashtree
- **Spec**: HTS-01 (https://github.com/mmalmi/hashtree/blob/master/docs/HTS-01.md)
- **Structure**: Hierarchical Merkle tree with MessagePack-encoded nodes
- **Mutable refs**: Kind 30078 Nostr events publish root hashes (NIP-33 replaceable)
- **Content addressing**: SHA-256 per node, deterministic encoding
- **Chunk size**: 2MB (optimized for Blossom uploads)
- **Max links per node**: 174 (configurable)
- **Encryption**: CHK (hash + key) by default
- **Implementations**: Rust CLI, TypeScript SDK, git remote helper
- **Pros**: Scalable, content-addressed, deduplicated, efficient updates, FUSE support exists
- **Cons**: More complex, requires tree traversal client-side

### 2. Blossom Drive (hzrd149/blossom-drive) — DEPRECATED
- **Repo**: https://github.com/hzrd149/blossom-drive
- **Structure**: Single kind 30563 event with all files listed as tags
- **File tag**: `["x", "<sha256>", "<absolute path>", "<size>", "<mime>"]`
- **Folder tag**: `["folder", "<path>"]`
- **Status**: Deprecated — "limited in how many files it can hold, does not scale well"
- **Replacement**: Bouquet (bouquet.slidestr.net)
- **Pros**: Simple, easy to query
- **Cons**: Doesn't scale (event size limits), deprecated

### 3. NIP-34 (Git) — INDIRECT
- Uses git's internal tree objects (not a custom format)
- Kind 30617 for repo announcements, 30618 for refs state
- Git-specific, not general-purpose

### 4. NIP-94 (File Metadata) — NO DIRECTORY SUPPORT
- Kind 1063 events with per-file metadata
- No directory or folder concept

## Recommendation for BlossomFS

### Simple Approach (PoC): Blossom Drive-style path tags
For a PoC, publish a kind 30078 event with `["x", sha256, path, size, mime]` tags.
This is simple, readable, and compatible with existing Blossom clients.
Limitation: doesn't scale beyond ~1000 files per event.

### Scalable Approach: Adopt Hashtree protocol
For production, use HTS-01 Merkle trees:
1. Build MessagePack tree nodes from the FUSE Tree
2. Upload tree chunks to Blossom server (2MB blobs)
3. Publish root hash via kind 30078 event
4. On mount, fetch root event, download tree nodes, reconstruct Tree

### Implementation steps (Hashtree path)
1. Add `rmp-serde` dep for MessagePack encoding
2. Implement tree node serialization matching HTS-01
3. Add `persist_tree()` — serialize tree → upload chunks → publish root event
4. Add `load_tree()` — fetch root event → download nodes → reconstruct Tree
5. Add `--persist` CLI flag to enable persistence
6. Cache tree nodes locally for fast remount

### Key design decisions
- Use kind 30078 with `["l", "blossomfs"]` tag for discoverability
- Support both public and encrypted trees
- On mount: check for existing root event, if found load tree, else build from BUD-12 list
- On unmount/write: persist updated tree

## References
- Hashtree spec: https://github.com/mmalmi/hashtree/blob/master/docs/HTS-01.md
- Blossom Drive spec: https://github.com/hzrd149/blossom-drive/blob/master/docs/drive.md
- NIP-34: https://github.com/nostr-protocol/nips/blob/master/34.md
- NIP-94: https://github.com/nostr-protocol/nips/blob/master/94.md
