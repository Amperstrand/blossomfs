# Research Validation

Validated specifications for BlossomFS against primary sources.
All BUD specs come from [hzrd149/blossom](https://github.com/hzrd149/blossom) (buds/ directory, master branch).
All NIP specs come from [nostr-protocol/nips](https://github.com/nostr-protocol/nips) (master branch).

---

## Crate Versions

Dependencies actually used in `Cargo.toml` as of the latest commit:

| Crate | Version | Role |
|---|---|---|
| `fuser` | 0.17 | FUSE3 filesystem (Linux) |
| `nostr-sdk` | 0.45.0-alpha.1 | Nostr relay client, key parsing, event signing |
| `reqwest` | 0.13 (stream feature) | HTTP client for BUD-02 upload, BUD-12 pagination, streaming blob fetch |
| `tokio` | 1 (full) | Async runtime |
| `clap` | 4 (derive) | CLI argument parsing |
| `serde` / `serde_json` | 1 | Serialization |
| `sha2` | 0.10 | SHA-256 hashing for blob verification and upload integrity |
| `hex` | 0.4 | Hex encoding/decoding |
| `base64` | 0.22 | Base64 encoding for BUD-11 auth headers |
| `thiserror` | 2 | Error handling |
| `tracing` | 0.1 | Structured logging |
| `tracing-subscriber` | 0.3 (env-filter) | Log filtering |
| `mime_guess` | 2 | MIME-to-extension inference |
| `wiremock` | 0.6 | HTTP mock server (dev-dep) |
| `tempfile` | 3 | Temporary directories (dev-dep) |

> **Note**: The project originally planned to use `nostr-blossom` and `directories` crates but ultimately replaced them with raw `nostr-sdk` + `reqwest` for more control, and a hardcoded `/tmp/blossomfs` cache default overridable via `--cache-dir`.

---

## BUD Specs

### BUD-01: Server Requirements and Blob Retrieval

**Status**: draft, mandatory
**Source**: [buds/01.md](https://github.com/hzrd149/blossom/blob/master/buds/01.md)

BUD-01 defines the core blob retrieval endpoints. All endpoints MUST be served from the root of the domain.

#### GET /\<sha256\> - Get Blob

Returns the blob contents in the response body. The server SHOULD set `Content-Type` to the appropriate MIME type, defaulting to `application/octet-stream` if unknown. Accepts an optional file extension suffix (`.pdf`, `.png`, etc.).

Servers MUST set `Access-Control-Allow-Origin: *` on all responses.

**Request**: No body. Path contains the 64-char lowercase hex SHA-256, optionally followed by a file extension.

**Response**:
- `Content-Type`: MIME type of the blob
- `Content-Length`: blob size in bytes
- `Accept-Ranges: bytes`: signals range request support

**Status codes**:

| Code | Meaning |
|---|---|
| 200 OK | Blob exists, returned in body |
| 206 Partial Content | Fulfilling a valid Range request |
| 307 Temporary Redirect | Blob available at another URL (must contain same sha256) |
| 308 Permanent Redirect | Blob permanently at another URL (must contain same sha256) |
| 400 Bad Request | Malformed sha256 or extension |
| 401 Unauthorized | Auth required and missing/invalid |
| 403 Forbidden | Request not allowed by server policy |
| 404 Not Found | Blob does not exist |
| 416 Range Not Satisfiable | Byte range invalid |
| 429 Too Many Requests | Rate limit exceeded |
| 503 Service Unavailable | Service temporarily down |

Servers MAY include `X-Reason` header on error responses (4xx/5xx) with a human-readable message.

Servers MAY include `Sunset` header to signal expected future unavailability.

#### HEAD /\<sha256\> - Has Blob

Identical to GET except no response body. Returns the same `Content-Type` and `Content-Length` headers.

**Status codes**: Same as GET, minus 206 and 416.

#### Range Requests

Servers SHOULD support RFC 7233 range requests on `GET /<sha256>` and signal support via `Accept-Ranges: bytes` on `HEAD /<sha256>`.

#### CORS Headers

For OPTIONS preflight requests, servers MUST set:
- `Access-Control-Allow-Headers: Authorization, *`
- `Access-Control-Allow-Methods: GET, HEAD, PUT, DELETE`

---

### BUD-02: Blob Upload and Blob Descriptor

**Status**: draft, optional
**Source**: [buds/02.md](https://github.com/hzrd149/blossom/blob/master/buds/02.md)

#### Blob Descriptor

A JSON object with these fields:

```json
{
  "url": "https://cdn.example.com/b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553.pdf",
  "sha256": "b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553",
  "size": 184292,
  "type": "application/pdf",
  "uploaded": 1725105921
}
```

| Field | Type | Description |
|---|---|---|
| `url` | string | Public URL to GET endpoint with file extension |
| `sha256` | string | 64-char lowercase hex SHA-256 of blob |
| `size` | integer | Size in bytes |
| `type` | string | MIME type (defaults to `application/octet-stream`) |
| `uploaded` | integer | Unix timestamp of upload |

Servers MAY include additional fields like `magnet`, `infohash`, or `ipfs`.

#### PUT /upload - Upload Blob

Accepts binary data in the request body. The server MUST NOT modify the blob and MUST compute the SHA-256 over the exact bytes received.

**Request headers** (SHOULD include):
- `Content-Type`: MIME type of the data
- `Content-Length`: size of the data
- `X-SHA-256` (optional): lowercase hex SHA-256 of the request body

**Response**: Blob Descriptor JSON in the body.

**Status codes**:

| Code | Meaning |
|---|---|
| 200 OK | Blob already existed, returning existing descriptor |
| 201 Created | Blob was newly stored, returning descriptor |
| 400 Bad Request | Malformed headers or body |
| 401 Unauthorized | Auth required and missing/invalid |
| 402 Payment Required | Payment required (BUD-07) |
| 403 Forbidden | Not allowed by server policy |
| 409 Conflict | X-SHA-256 does not match body |
| 411 Length Required | Content-Length header missing |
| 413 Content Too Large | Exceeds server size limits |
| 415 Unsupported Media Type | Type not supported |
| 429 Too Many Requests | Rate limit exceeded |

Servers MAY normalize file extensions based on the MIME type.

---

### BUD-03: User Server List

**Status**: draft, optional
**Source**: [buds/03.md](https://github.com/hzrd149/blossom/blob/master/buds/03.md)

Defines a replaceable Nostr event (kind 10063) advertising a user's Blossom servers.

#### Kind 10063 Event

```json
{
  "id": "e4bee088334cb5d38cff1616e964369c37b6081be997962ab289d6c671975d71",
  "pubkey": "781208004e09102d7da3b7345e64fd193cd1bc3fce8fdae6008d77f9cabcd036",
  "content": "",
  "kind": 10063,
  "created_at": 1708774162,
  "tags": [
    ["server", "https://cdn.self.hosted"],
    ["server", "https://cdn.satellite.earth"]
  ],
  "sig": "..."
}
```

- `.content` is unused (empty string).
- Each `server` tag contains a full URL including `http://` or `https://`.
- Tag order matters: most reliable/trusted servers first.

#### Client Retrieval Strategy

When a blob URL is unavailable, a client should:
1. Extract the SHA-256 from the URL (use the **last** 64-char hex string occurrence).
2. Look up the author's kind 10063 server list.
3. Try each server in order, constructing `GET /<sha256>` URLs.
4. Fall back to well-known public Blossom servers if no server list exists.

This extraction strategy works with both Blossom URLs and many non-Blossom URL patterns.

---

### BUD-11: Nostr Authorization

**Status**: draft, optional
**Source**: [buds/11.md](https://github.com/hzrd149/blossom/blob/master/buds/11.md)

Defines authorization tokens as signed Nostr events of kind 24242.

#### Kind 24242 Authorization Event

```json
{
  "kind": 24242,
  "pubkey": "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
  "created_at": 1772019044,
  "tags": [
    ["t", "upload"],
    ["expiration", "1708858680"],
    ["x", "b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553"]
  ],
  "content": "Upload Blob",
  "sig": "..."
}
```

**Required tags**:
- `t`: action verb. One of `get`, `upload`, `list`, `delete`, `media`. MUST match the endpoint action.
- `expiration`: NIP-40 expiration as Unix timestamp. MUST be in the future.

**Optional scoping tags**:
- `server`: lowercase domain name only (e.g. `cdn.example.com`). Limits token to specific servers.
- `x`: lowercase hex SHA-256 blob hash. Limits token to specific blobs.

**Content**: Human-readable string describing the intended use.

#### HTTP Authorization Header

Format: `Authorization: Nostr <base64url-nopad-event>`

The event is encoded as Base64 URL-safe without padding (same as JWT encoding).

```
Authorization: Nostr ewogICJraW5kIjogMjQyNDIsIC4uLn0=
```

#### Endpoint Authorization Requirements

| Endpoint | Required `t` tag | Implied blob hash | `x` tag required? |
|---|---|---|---|
| `GET /<sha256>` | `get` | URL path | optional |
| `HEAD /<sha256>` | `get` | URL path | optional |
| `PUT /upload` | `upload` | `X-SHA-256` header | required |
| `HEAD /upload` | `upload` | `X-SHA-256` header | required |
| `DELETE /<sha256>` | `delete` | URL path | required |
| `GET /list/<pubkey>` | `list` | none | N/A |
| `PUT /mirror` | `upload` | mirrored blob hash | required |
| `PUT /media` | `media` | `X-SHA-256` header | required |
| `HEAD /media` | `media` | `X-SHA-256` header | required |

#### Validation Rules

Servers MUST check:
1. Event kind is 24242.
2. `created_at` is in the past.
3. `expiration` tag present and in the future.
4. `t` tag matches the endpoint action.
5. If `server` tags present, server domain must match at least one.
6. If endpoint requires `x` tags and they are present, at least one must match.

#### Security Note

Unscoped tokens (no `server` tag) are valid on any Blossom server. This is especially risky for `delete` tokens: an unscoped delete token intercepted from one server can be replayed against any other server.

---

### BUD-12: Blob Management Endpoints

**Status**: draft, optional
**Source**: [buds/12.md](https://github.com/hzrd149/blossom/blob/master/buds/12.md)

Defines the `/list/<pubkey>` and `DELETE /<sha256>` endpoints.

#### GET /list/\<pubkey\> - List Blobs

Returns a JSON array of Blob Descriptors uploaded by the specified pubkey.

**Note**: This endpoint is **unrecommended** and **optional**. Servers MAY implement it but are not required to.

**Query parameters**:
- `cursor` (string): SHA-256 of the last blob in the previous page. Omit for the first page.
- `limit` (integer): Maximum number of results to return.
- `since` (integer, deprecated): Filter by minimum `uploaded` timestamp.
- `until` (integer, deprecated): Filter by maximum `uploaded` timestamp.

Results MUST be sorted by `uploaded` date descending. The blob at the cursor position MUST NOT be included in the results.

**Response**: JSON array of Blob Descriptor objects (same format as BUD-02).

**Status codes**:

| Code | Meaning |
|---|---|
| 200 OK | Success, body contains array of descriptors |
| 400 Bad Request | Malformed query parameters |
| 401 Unauthorized | Auth required and missing/invalid |
| 402 Payment Required | Payment required |
| 403 Forbidden | Not allowed by policy |
| 429 Too Many Requests | Rate limit exceeded |
| 503 Service Unavailable | Service temporarily down |

#### DELETE /\<sha256\> - Delete Blob

**Status codes**:

| Code | Meaning |
|---|---|
| 200 OK | Blob deleted (may include body) |
| 204 No Content | Blob deleted (no body) |
| 401 Unauthorized | Auth required and missing/invalid |
| 402 Payment Required | Payment required |
| 403 Forbidden | Not allowed by policy |
| 404 Not Found | Blob does not exist |
| 429 Too Many Requests | Rate limit exceeded |
| 503 Service Unavailable | Service temporarily down |

Multiple `x` tags in the auth token MUST NOT be interpreted as a batch delete request.

---

## NIP Specs

### NIP-94: File Metadata (Kind 1063)

**Status**: draft, optional
**Source**: [nips/94.md](https://github.com/nostr-protocol/nips/blob/master/94.md)

Defines a Nostr event kind 1063 for file metadata classification. Support is NOT expected from social clients (kind 1) or longform clients (kind 30023).

#### Event Structure

- `kind`: 1063
- `content`: File description / caption

**Tags**:

| Tag | Required | Description |
|---|---|---|
| `url` | yes | Download URL for the file |
| `m` | yes | MIME type (lowercase) |
| `x` | yes | SHA-256 hex of the file |
| `ox` | no | SHA-256 hex of original file (before server transforms) |
| `size` | no | File size in bytes |
| `dim` | no | Dimensions in `<width>x<height>` format |
| `magnet` | no | Magnet URI |
| `i` | no | Torrent infohash |
| `blurhash` | no | Blurhash for loading placeholder |
| `thumb` | no | Thumbnail URL, optionally followed by its SHA-256 |
| `image` | no | Preview image URL, optionally followed by its SHA-256 |
| `summary` | no | Text excerpt |
| `alt` | no | Accessibility description |
| `fallback` | no | Zero or more fallback sources if `url` fails |
| `service` | no | Service type serving the file (e.g. NIP-96) |

#### Example

```json
{
  "kind": 1063,
  "content": "Screenshot of the new UI",
  "tags": [
    ["url", "https://cdn.example.com/abc123...def.png"],
    ["m", "image/png"],
    ["x", "b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553"],
    ["size", "477328"],
    ["dim", "1920x1080"],
    ["alt", "New UI screenshot"]
  ]
}
```

---

### NIP-B7: Blossom Media

**Status**: draft, optional
**Source**: [nips/B7.md](https://github.com/nostr-protocol/nips/blob/master/B7.md)

NIP-B7 specifies how Nostr clients should integrate Blossom for media handling. It is a thin integration NIP that primarily cross-references BUD-01 and BUD-03.

**Key requirements**:
- Clients SHOULD fetch kind 10063 (BUD-03) server lists for each user.
- When a media URL ending in a 64-char hex string is unavailable, clients SHOULD look up the author's kind 10063 list and try alternative servers.
- Clients SHOULD verify that the SHA-256 hash of downloaded content matches the hex string from the URL.

NIP-B7 does not define any new event kinds or HTTP endpoints. It simply tells Nostr client developers to use BUD-03 for server discovery and BUD-01 for blob retrieval.

---

### Blossom Drive (Kind 30563)

**Status**: deprecated / abandoned
**Source**: [hzrd149/blossom-drive/docs/drive.md](https://github.com/hzrd149/blossom-drive/blob/master/docs/drive.md)

Blossom Drive was an experimental project by hzrd149 for creating folder structures over Blossom blobs. The project is no longer maintained. The author explicitly states it should be completely redesigned to scale.

#### Kind 30563 Event

A replaceable Nostr event defining a drive (collection of blobs in a folder structure).

**Notable tags**:

| Tag | Description |
|---|---|
| `d` | Drive identifier (parameterized replaceable events) |
| `name` | Drive name |
| `description` | Short description |
| `server` (multiple) | Preferred Blossom servers for downloading blobs |
| `x` | File entry |
| `folder` | Empty folder placeholder |

**The `x` tag format**: `["x", "<sha256>", "<absolute-path>", "<size>", "<mime>"]`

```json
["x", "b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553", "/bitcoin.pdf", "184292", "application/pdf"]
```

**The `folder` tag format**: `["folder", "<path>"]`

```json
["folder", "/documents"]
```

Folder tags act as placeholders for empty directories. Once a file is placed at a path that would create that directory, the folder tag can be removed.

**Design limitation**: The single-event design does not scale. All files in a drive live in one replaceable event, which limits the number of files and does not handle concurrent edits well. The author recommends an append-only log approach instead.

#### Encrypted variant: Kind 30564

An encrypted drive variant uses kind 30564 with password-based encryption. Not relevant to BlossomFS (read-only, public blobs only).

---

## Corrections

Discrepancies discovered between initial assumptions and validated primary sources.

### fuser 0.17.0 Callback Signature

**Assumption**: FUSE callbacks take `&mut self`, allowing direct mutation of filesystem state.

**Reality**: fuser 0.17.0 uses `&self` for all `Filesystem` trait callbacks, not `&mut self`. This is a fundamental design constraint. Mutable state (like the content cache used for lazy blob fetching) cannot be accessed through a direct mutable reference.

**Impact on BlossomFS**: Interior mutability via `Arc<Mutex<CacheState>>` is required for the content cache. The directory tree itself can remain immutable (built once at mount time), so `Arc<Tree>` without a Mutex suffices for the tree structure.

---

### nostr-blossom 0.45.0-alpha.1: No Cursor Pagination

**Assumption**: `nostr-blossom` crate provides a `list_blobs()` method with cursor-based pagination matching the BUD-12 spec.

**Reality**: The `nostr-blossom` crate does expose `list_blobs()`, but it does NOT implement cursor-based pagination as defined by BUD-12 (which specifies `cursor` and `limit` query parameters). The crate only supports `since` and `until` filters, which BUD-12 explicitly marks as deprecated for pagination purposes.

**Impact on BlossomFS**: We cannot rely on `nostr-blossom` for efficient paginated listing. Instead, we use raw `reqwest` calls for BUD-12 listing with proper `cursor`/`limit` parameters, and fall back to `since`/`until` only when cursor pagination is not supported by the server.

---

### nostr-blossom 0.45.0-alpha.1: Full-Blob Memory Loading

**Assumption**: `nostr-blossom`'s `get_blob()` streams or lazily loads blob data.

**Reality**: `get_blob()` returns `Vec<u8>`, loading the entire blob into memory. For large files (images, videos, PDFs that Blossom is commonly used for), this is an OOM risk. BlossomFS cannot afford to load multi-gigabyte blobs into memory.

**Impact on BlossomFS**: We use raw `reqwest` with streaming (`reqwest::Response::bytes_stream()`) for blob retrieval. This allows us to stream data directly to the cache file on disk while computing the SHA-256 hash in a streaming fashion, with a bounded memory footprint regardless of blob size.

---

### Hybrid Client Decision

**Summary**: BlossomFS uses a hybrid approach for the Blossom client layer.

| Concern | Crate | Reason |
|---|---|---|
| Type definitions (BlobDescriptor, event kinds) | `nostr-blossom` | Maintained types, aligned with nostr-sdk version |
| BUD-12 paginated listing | `reqwest` | nostr-blossom lacks cursor pagination |
| BUD-01 blob retrieval | `reqwest` | nostr-blossom loads full blob into memory (OOM risk) |
| Auth event construction (kind 24242) | `nostr-sdk` / `nostr-blossom` | Event signing with nostr keys |

This means `nostr-blossom` is used for types and signing, while `reqwest` handles all HTTP I/O that requires pagination or streaming.

---

### BUD-12 Listing is Optional and Unrecommended

**Assumption**: All Blossom servers implement `GET /list/<pubkey>`.

**Reality**: BUD-12 explicitly marks the list endpoint as "unrecommended" and states servers are not required to implement it. A functioning Blossom server may only expose GET, HEAD, PUT, and DELETE endpoints.

**Impact on BlossomFS**: The filesystem should gracefully handle servers that do not support listing. It should fall back to kind 10063 discovery plus HEAD-based existence checks, or to NIP-94 / Blossom Drive metadata for building the directory tree.

---

### BUD-11 Auth Header Name

**Assumption**: Authorization uses `X-Blossom-Authorization` header.

**Reality**: BUD-11 specifies the standard HTTP `Authorization` header with the `Nostr` scheme: `Authorization: Nostr <base64url-nopad-event>`. There is no `X-Blossom-Authorization` header in the spec.

**Impact on BlossomFS**: The auth module correctly implements `Authorization: Nostr ...` (confirmed in `src/blossom/auth.rs` doc comment).

---

## Primary Source URLs

| Spec | URL |
|---|---|
| BUD-00 | https://github.com/hzrd149/blossom/blob/master/buds/00.md |
| BUD-01 | https://github.com/hzrd149/blossom/blob/master/buds/01.md |
| BUD-02 | https://github.com/hzrd149/blossom/blob/master/buds/02.md |
| BUD-03 | https://github.com/hzrd149/blossom/blob/master/buds/03.md |
| BUD-05 | https://github.com/hzrd149/blossom/blob/master/buds/05.md |
| BUD-06 | https://github.com/hzrd149/blossom/blob/master/buds/06.md |
| BUD-11 | https://github.com/hzrd149/blossom/blob/master/buds/11.md |
| BUD-12 | https://github.com/hzrd149/blossom/blob/master/buds/12.md |
| NIP-94 | https://github.com/nostr-protocol/nips/blob/master/94.md |
| NIP-B7 | https://github.com/nostr-protocol/nips/blob/master/B7.md |
| Blossom Drive | https://github.com/hzrd149/blossom-drive/blob/master/docs/drive.md |
| Blossom repo | https://github.com/hzrd149/blossom |
| NIPs repo | https://github.com/nostr-protocol/nips |
