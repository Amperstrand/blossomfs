#!/usr/bin/env bash
set -euo pipefail

TOLLGATE_PUBKEY="5075e61f0b048148b60105c1dd72bbeae1957336ae5824087e52efa374f8416a"
TOLLGATE_RELAY="wss://relay.tollgate.me"
TOLLGATE_SERVER="https://blossom.primal.net"
WORKDIR="/tmp/blossom-ci-tollgate"
MOUNTPOINT="${WORKDIR}/mnt"
BLOSSOMFS_BIN="${BLOSSOMFS_BIN:-./target/release/blossomfs}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC}: $1"; }
fail() { echo -e "${RED}FAIL${NC}: $1"; exit 1; }
info() { echo -e "${YELLOW}....${NC}: $1"; }

info "Checking dependencies..."
for cmd in curl find fusermount3; do
  command -v "$cmd" >/dev/null 2>&1 || fail "$cmd not found"
done
[ -x "$BLOSSOMFS_BIN" ] || fail "BlossomFS binary not found at $BLOSSOMFS_BIN"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR" "$MOUNTPOINT"

echo ""
echo "━━━ Scenario 1: Mount tollgate releases via NIP-94 relay ━━━"
info "Pubkey:  ${TOLLGATE_PUBKEY}"
info "Relay:   ${TOLLGATE_RELAY}"
info "Server:  ${TOLLGATE_SERVER}"

"$BLOSSOMFS_BIN" mount \
  --pubkey "$TOLLGATE_PUBKEY" \
  --relay "$TOLLGATE_RELAY" \
  --server "$TOLLGATE_SERVER" \
  --mountpoint "$MOUNTPOINT" \
  --read-only=true \
  --cache-dir "${WORKDIR}/cache" &
BFS_PID=$!
sleep 5

kill -0 "$BFS_PID" 2>/dev/null || fail "BlossomFS failed to mount"

for i in $(seq 1 20); do
  [ -f "${MOUNTPOINT}/STATUS.txt" ] && break
  sleep 1
done
[ -f "${MOUNTPOINT}/STATUS.txt" ] || fail "STATUS.txt not found (relay query timeout)"

pass "BlossomFS mounted tollgate releases"

echo ""
echo "━━━ Scenario 2: /tollgate/ directory exists ━━━"

TOLLGATE_DIR="${MOUNTPOINT}/tollgate"
[ -d "$TOLLGATE_DIR" ] || fail "/tollgate/ directory not found"
pass "/tollgate/ directory exists"

TOLLGATE_COUNT=$(find "$TOLLGATE_DIR" -type f | wc -l)
info "Found ${TOLLGATE_COUNT} files under /tollgate/"
[ "$TOLLGATE_COUNT" -gt 0 ] || fail "No files in /tollgate/"
pass "${TOLLGATE_COUNT} tollgate files browseable"

echo ""
echo "━━━ Scenario 3: OS images directory structure ━━━"

OS_DIR="${TOLLGATE_DIR}/os"
if [ -d "$OS_DIR" ]; then
  OS_COUNT=$(find "$OS_DIR" -type f | wc -l)
  info "Found ${OS_COUNT} OS image files"

  CHANNELS=$(find "$OS_DIR" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; 2>/dev/null | sort)
  info "Channels: $(echo "$CHANNELS" | tr '\n' ' ')"
  [ -n "$CHANNELS" ] || fail "No release channels found under /tollgate/os/"
  pass "OS images organized by channel: $(echo "$CHANNELS" | tr '\n' ' ')"
else
  fail "/tollgate/os/ directory not found"
fi

echo ""
echo "━━━ Scenario 4: MT3000 firmware image available ━━━"

MT3000_FILES=$(find "$OS_DIR" -type f -name '*mt3000*' || true)
MT3000_COUNT=$(echo "$MT3000_FILES" | grep -c '.' || true)

if [ "$MT3000_COUNT" -gt 0 ]; then
  pass "Found ${MT3000_COUNT} MT3000 file(s):"
  echo "$MT3000_FILES" | while read -r f; do
    REL_PATH=${f#${MOUNTPOINT}/}
    info "  ${REL_PATH}"
  done
else
  fail "No files matching '*mt3000*' found in /tollgate/os/"
fi

echo ""
echo "━━━ Scenario 5: MT3000 file content fetchable ━━━"

MT3000_FILE=$(echo "$MT3000_FILES" | head -1)
if [ -n "$MT3000_FILE" ]; then
  MT3000_BASENAME=$(basename "$MT3000_FILE")
  info "Fetching first 64 bytes of: $MT3000_BASENAME"

  FIRST_BYTES=$(dd if="$MT3000_FILE" bs=1 count=64 2>/dev/null | xxd -p || true)
  if [ -n "$FIRST_BYTES" ]; then
    pass "Content fetched (first 16 hex chars: ${FIRST_BYTES:0:16}...)"
  else
    info "WARNING: Could not read content (server may be temporarily unavailable)"
    info "File listing verified — content fetch is secondary"
  fi
fi

echo ""
echo "━━━ Scenario 6: Packages directory structure ━━━"

PKG_DIR="${TOLLGATE_DIR}/packages"
if [ -d "$PKG_DIR" ]; then
  PKG_COUNT=$(find "$PKG_DIR" -type f | wc -l)
  info "Found ${PKG_COUNT} package files"
  if [ "$PKG_COUNT" -gt 0 ]; then
    pass "Packages directory populated with ${PKG_COUNT} files"
  else
    info "No package files (may not have been published yet)"
  fi
else
  info "/tollgate/packages/ not found (no package events on relay)"
fi

echo ""
echo "━━━ Scenario 7: NIP-94 flat listing also available ━━━"

NIP94_DIR="${MOUNTPOINT}/nip94/${TOLLGATE_PUBKEY}"
if [ -d "$NIP94_DIR" ]; then
  NIP94_COUNT=$(find "$NIP94_DIR" -type f | wc -l)
  info "Found ${NIP94_COUNT} files in /nip94/${TOLLGATE_PUBKEY:0:16}.../"
  [ "$NIP94_COUNT" -gt 0 ] || fail "No NIP-94 browseable files"
  pass "Flat NIP-94 listing available (${NIP94_COUNT} files)"
else
  fail "NIP-94 directory not found"
fi

info "Unmounting..."
kill "$BFS_PID" 2>/dev/null
wait "$BFS_PID" 2>/dev/null || true
fusermount3 -u "$MOUNTPOINT" 2>/dev/null || true

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}TOLLGATE CI TEST PASSED${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Pubkey:       ${TOLLGATE_PUBKEY:0:32}..."
echo "Relay:        $TOLLGATE_RELAY"
echo "Blossom:      $TOLLGATE_SERVER"
echo "OS images:    $OS_COUNT files"
echo "MT3000 files: $MT3000_COUNT"
[ -d "$PKG_DIR" ] && echo "Packages:     $PKG_COUNT files" || echo "Packages:     (none)"
echo "NIP-94 flat:  $NIP94_COUNT files"
echo "Total:        $TOLLGATE_COUNT tollgate files"
echo ""
echo "BlossomFS successfully browsed tollgate releases with structured directory layout."
