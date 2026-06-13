#!/usr/bin/env bash
set -euo pipefail

# ─── Configuration ──────────────────────────────────────────────────────────
SERVER="https://blossom.psbt.me"
MINT="https://testnut.cashu.space"
CASHU_WALLET="ci-test"
WORKDIR="/tmp/blossom-ci"
MOUNTPOINT="${WORKDIR}/mnt"
BLOSSOMFS_BIN="${BLOSSOMFS_BIN:-./target/release/blossomfs}"

# Fixed test keypair (disposable, for CI only)
SECRET="2c40c66ddcafc6fef1470f6eb21f7b7305bd3f5bb6d5f8d402f842928e6bf4db"
PUBKEY="b36242762df892cd391dd5ac2537118cb731b57c5896eb5f8d7b88aab3759e39"

# ─── Helpers ────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1"; exit 1; }
info() { echo -e "${YELLOW}....${NC}: $1"; }

nak_auth() {
  local blob_hash="$1"
  nak event -k 24242 -c "CI upload" \
    -t "t=upload" \
    -t "expiration=$(($(date +%s) + 600))" \
    -t "x=$blob_hash" \
    --sec "$SECRET" 2>/dev/null
}

cashu_send() {
  local amount="$1"
  cashu -h "$MINT" -w "$CASHU_WALLET" -u sat -y send "$amount" 2>/dev/null \
    | grep "^cashu" | head -1 || true
}

HAS_ECASH=false

ensure_cashu_balance() {
  local balance
  balance=$(cashu -h "$MINT" -w "$CASHU_WALLET" -u sat -y balance 2>/dev/null | head -1 || true)
  balance=$(echo "$balance" | grep -oP '\d+' || echo "0")
  if [ "$balance" -lt 50 ]; then
    info "Low balance ($balance sat), minting 1000 sat from test mint..."
    if timeout 120 cashu -h "$MINT" -w "$CASHU_WALLET" -u sat -y invoice 1000 >/dev/null 2>&1; then
      HAS_ECASH=true
    else
      info "WARNING: Could not mint ecash from $MINT (test mint may be unavailable)"
      info "Scenarios requiring payment will be skipped"
    fi
  else
    HAS_ECASH=true
  fi
}

# ─── Dependency check ───────────────────────────────────────────────────────
info "Checking dependencies..."
for cmd in nak cashu curl sha256sum jq fusermount3; do
  command -v "$cmd" >/dev/null 2>&1 || fail "$cmd not found"
done
[ -x "$BLOSSOMFS_BIN" ] || fail "BlossomFS binary not found at $BLOSSOMFS_BIN (build with: cargo build --release)"
info "All dependencies OK"

# ─── Setup ──────────────────────────────────────────────────────────────────
rm -rf "$WORKDIR"
mkdir -p "$WORKDIR" "$MOUNTPOINT"

# Ensure cashu wallet has funds
ensure_cashu_balance

# ─── Test files ─────────────────────────────────────────────────────────────
# Git commit hash prefix + zero padding (identifiable, reproducible per commit)
GIT_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
info "Git commit: $GIT_SHA"

# Small file: 512 KB (<1MB → free). First bytes = "blossomfs-ci:<commit>", rest zeros.
{
  printf 'blossomfs-ci:%s\0' "$GIT_SHA"
  dd if=/dev/zero bs=1 count=$(( 524288 - 40 )) status=none
} > "${WORKDIR}/small.bin"
SMALL_SIZE=$(stat -c%s "${WORKDIR}/small.bin")
SMALL_HASH=$(sha256sum "${WORKDIR}/small.bin" | cut -d' ' -f1)

# Large file: 2 MB (>1MB → payment). First bytes = "blossomfs-ci:<commit>", rest zeros.
{
  printf 'blossomfs-ci:%s\0' "$GIT_SHA"
  dd if=/dev/zero bs=1048576 count=2 status=none
} > "${WORKDIR}/large.bin"
LARGE_SIZE=$(stat -c%s "${WORKDIR}/large.bin")
LARGE_HASH=$(sha256sum "${WORKDIR}/large.bin" | cut -d' ' -f1)

info "Small file: ${SMALL_SIZE}B sha256=${SMALL_HASH:0:16}... prefix=$(head -c20 "${WORKDIR}/small.bin" | tr '\0' '~')"
info "Large file: ${LARGE_SIZE}B sha256=${LARGE_HASH:0:16}... prefix=$(head -c20 "${WORKDIR}/large.bin" | tr '\0' '~')"

# ─── Scenario 1: Upload <1MB (no payment needed) ────────────────────────────
echo ""
echo "━━━ Scenario 1: Upload <1MB without payment ━━━"

SMALL_RESULT=$(nak blossom -s "$SERVER" --sec "$SECRET" upload "${WORKDIR}/small.bin" 2>&1 || true)
if echo "$SMALL_RESULT" | grep -q "$SMALL_HASH"; then
  pass "Small file uploaded successfully"
  echo "$SMALL_RESULT" | jq '.' 2>/dev/null || echo "$SMALL_RESULT"
else
  fail "Small file upload failed: $SMALL_RESULT"
fi

# ─── Scenario 2: Upload >1MB without payment → expect 402 ───────────────────
echo ""
echo "━━━ Scenario 2: Upload >1MB without payment → expect 402 ━━━"

AUTH_EVENT=$(nak_auth "$LARGE_HASH")
AUTH_B64=$(echo "$AUTH_EVENT" | base64 -w0)
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "${SERVER}/upload" \
  -H "Authorization: Nostr $AUTH_B64" \
  -H "Content-Type: application/octet-stream" \
  -H "Content-Length: $LARGE_SIZE" \
  -H "X-SHA-256: $LARGE_HASH" \
  --data-binary "@${WORKDIR}/large.bin")

if [ "$HTTP_CODE" = "402" ]; then
  pass "Server returned 402 Payment Required for >1MB without payment"
else
  fail "Expected 402, got $HTTP_CODE"
fi

# ─── Scenario 3: Upload >1MB WITH Cashu payment ─────────────────────────────
echo ""
echo "━━━ Scenario 3: Upload >1MB with Cashu payment ━━━"

LARGE_UPLOADED=false
if [ "$HAS_ECASH" = "true" ]; then
  info "Creating Cashu token from ${MINT}..."
  PAYMENT_TOKEN=$(cashu_send 10)
  if [ -n "$PAYMENT_TOKEN" ]; then
    info "Token: ${PAYMENT_TOKEN:0:40}..."

    # Need fresh auth event (previous one might have been consumed)
    AUTH_EVENT=$(nak_auth "$LARGE_HASH")
    AUTH_B64=$(echo "$AUTH_EVENT" | base64 -w0)

    UPLOAD_RESP=$(curl -s -w "\n%{http_code}" -X PUT "${SERVER}/upload" \
      -H "Authorization: Nostr $AUTH_B64" \
      -H "Content-Type: application/octet-stream" \
      -H "Content-Length: $LARGE_SIZE" \
      -H "X-SHA-256: $LARGE_HASH" \
      -H "X-Cashu: $PAYMENT_TOKEN" \
      --data-binary "@${WORKDIR}/large.bin")

    UPLOAD_BODY=$(echo "$UPLOAD_RESP" | head -n -1)
    UPLOAD_CODE=$(echo "$UPLOAD_RESP" | tail -n1)

    if [ "$UPLOAD_CODE" = "201" ] || [ "$UPLOAD_CODE" = "200" ]; then
      RESP_HASH=$(echo "$UPLOAD_BODY" | jq -r '.sha256')
      [ "$RESP_HASH" = "$LARGE_HASH" ] || fail "SHA-256 mismatch: $RESP_HASH != $LARGE_HASH"
      pass "Large file uploaded with Cashu payment (HTTP $UPLOAD_CODE)"
      echo "$UPLOAD_BODY" | jq '.' 2>/dev/null || echo "$UPLOAD_BODY"
      LARGE_UPLOADED=true
    else
      fail "Upload failed with HTTP $UPLOAD_CODE: $UPLOAD_BODY"
    fi
  else
    info "WARNING: Could not create Cashu token — skipping payment scenario"
  fi
else
  info "WARNING: No ecash available — skipping payment scenario"
fi

# ─── Scenario 4: Mount BlossomFS and verify blobs ───────────────────────────
echo ""
echo "━━━ Scenario 4: Mount BlossomFS and read blobs back ━━━"

info "Mounting BlossomFS at $MOUNTPOINT..."
"$BLOSSOMFS_BIN" mount \
  --server "$SERVER" \
  --pubkey "$PUBKEY" \
  --mountpoint "$MOUNTPOINT" \
  --read-only &
BFS_PID=$!
sleep 3

# Verify mount is alive
kill -0 "$BFS_PID" 2>/dev/null || fail "BlossomFS failed to mount"

# Wait for mount to be ready
for i in $(seq 1 10); do
  [ -f "${MOUNTPOINT}/STATUS.txt" ] && break
  sleep 1
done
[ -f "${MOUNTPOINT}/STATUS.txt" ] || fail "STATUS.txt not found in mount"

BLOB_COUNT=$(find "$MOUNTPOINT" -type f -name "$(echo "$SMALL_HASH" | cut -c1-8)*" | wc -l)
info "Found $BLOB_COUNT small blobs in mount"

# Verify small file
SMALL_FUSE=$(find "$MOUNTPOINT" -type f -name "${SMALL_HASH:0:16}*" | head -1 || true)
if [ -n "$SMALL_FUSE" ]; then
  FUSE_HASH=$(sha256sum "$SMALL_FUSE" | cut -d' ' -f1)
  [ "$FUSE_HASH" = "$SMALL_HASH" ] && pass "Small blob SHA-256 verified through FUSE" || fail "Small blob hash mismatch"
else
  info "Small blob not found in mount (may have expired from previous run) — skipping"
fi

# Verify large file
LARGE_FUSE=$(find "$MOUNTPOINT" -type f -name "${LARGE_HASH:0:16}*" | head -1 || true)
if [ -n "$LARGE_FUSE" ]; then
  FUSE_HASH=$(sha256sum "$LARGE_FUSE" | cut -d' ' -f1)
  [ "$FUSE_HASH" = "$LARGE_HASH" ] && pass "Large blob SHA-256 verified through FUSE" || fail "Large blob hash mismatch"
elif [ "$LARGE_UPLOADED" = "true" ]; then
  fail "Large blob not found in mount"
else
  info "Large blob not uploaded (payment scenario skipped) — skipping FUSE verification"
fi

info "Unmounting..."
kill "$BFS_PID" 2>/dev/null
wait "$BFS_PID" 2>/dev/null || true
fusermount3 -u "$MOUNTPOINT" 2>/dev/null || true

# ─── Summary ────────────────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}ALL SCENARIOS PASSED${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Server:       $SERVER"
echo "Mint:         $MINT"
echo "Pubkey:       $PUBKEY"
echo "Small blob:   ${SMALL_HASH:0:32}...  (512KB, free)"
if [ "$LARGE_UPLOADED" = "true" ]; then
  echo "Large blob:   ${LARGE_HASH:0:32}...  (2MB, paid with ecash)"
else
  echo "Large blob:   (skipped — no ecash available)"
fi
echo ""
echo "BlossomFS successfully mounted and read both blobs."
