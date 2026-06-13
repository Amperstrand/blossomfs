# CI Cashu Integration Test — Known Good State

**Date**: 2026-06-13
**Commit**: `2259f1c` + CI test commits
**Server**: `blossom.psbt.me`
**Mint**: `testnut.cashu.space`

## Overview

BlossomFS was tested end-to-end against a live Blossom server with Cashu ecash payments. Four scenarios cover the complete upload → payment → mount → read lifecycle.

## Prerequisites

```bash
# Tools
go install github.com/fiatjaf/nak@latest      # Nostr Army Knife (BUD-11 auth, blossom upload)
pip install cashu                              # Cashu wallet CLI
sudo apt-get install -y fuse3 libfuse3-dev     # FUSE3

# BlossomFS
cargo build --release

# Fund the CI Cashu wallet (test mint, free)
cashu -h https://testnut.cashu.space -w ci-test -u sat -y invoice 1000
```

## Test Scenarios

### Scenario 1: Upload <1MB (free, no payment)

Files under 1MB are stored for free. `nak blossom upload` handles BUD-11 auth automatically.

```
nak blossom -s https://blossom.psbt.me --sec <key> upload small.bin
→ 200 OK, blob descriptor returned
```

### Scenario 2: Upload >1MB without payment → 402

Files over 1MB require Cashu payment (BUD-07). Without payment, the server returns `402 Payment Required` with headers:

```
X-Cashu: creqAuQADYWEEYXVjc2F0YW2BeB5odHRwczovL3Rlc3RudXQuY2FzaHUuZXhjaGFuZ2U
X-Price-Sats: 4
```

The `X-Cashu` header contains a NUT-24 request token specifying the amount (4 sats) and accepted mint URL.

### Scenario 3: Upload >1MB with Cashu payment

Payment flow:
1. Decode the `X-Cashu` request token (amount + mint URL)
2. Create a Cashu token (ecash proofs) from the specified mint
3. Retry `PUT /upload` with `X-Cashu: <cashuB token>` header
4. Server melts the ecash and stores the blob

```bash
# Create payment token from test mint
PAYMENT=$(cashu -h https://testnut.cashu.space -w ci-test -u sat -y send 10 | grep "^cashu")

# Upload with payment
curl -X PUT https://blossom.psbt.me/upload \
  -H "Authorization: Nostr <base64-event>" \
  -H "X-SHA-256: <hash>" \
  -H "X-Cashu: $PAYMENT" \
  --data-binary @large.bin
→ 201 Created
```

The server accepts ecash from `testnut.cashu.space` even though the `X-Cashu` request token references `testnut.cashu.exchange` — both are operated by the same entity.

### Scenario 4: Mount BlossomFS and verify blobs

Both blobs (free + paid) appear in the BUD-12 listing and are readable through the FUSE mount:

```
./target/release/blossomfs mount \
  --server https://blossom.psbt.me \
  --pubkey <hex-pubkey> \
  --mountpoint /tmp/mnt \
  --read-only

# SHA-256 verified through FUSE read-back
sha256sum /tmp/mnt/public/<pubkey>/servers/blossom.psbt.me/by-sha256/<hash>
→ matches original file
```

## Test Results (2026-06-13)

| Scenario | Description | Result |
|----------|-------------|--------|
| 1 | Upload 512KB without payment | PASS (200 OK) |
| 2 | Upload 2MB without payment | PASS (402 Payment Required) |
| 3 | Upload 2MB with Cashu payment | PASS (201 Created, 4 sats) |
| 4 | Mount BlossomFS, verify SHA-256 | PASS (both blobs match) |

**Cost**: 4 sats (test ecash, no real value) for 2MB stored ~7-14 days.
**Speed**: Full test completes in ~15 seconds including mount + SHA-256 verification.

## Running the Test

```bash
export PATH="$HOME/go/bin:$HOME/.local/bin:$PATH"
cd blossomfs
bash scripts/ci_test.sh
```

The script is idempotent — it generates new test files each run and verifies against the live server.

## Architecture Notes

- **nak** handles BUD-11 auth event creation and signing (kind 24242)
- **curl** handles the paid upload (nak doesn't support Cashu payment headers)
- **cashu CLI** manages the ecash wallet and creates payment tokens
- **BlossomFS** mounts the server read-only and verifies blob integrity via SHA-256
- All HTTP traffic goes through Cloudflare — Python's urllib is blocked (error 1010), but curl and nak work fine
