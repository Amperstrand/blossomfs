#!/usr/bin/env bash
set -euo pipefail

NGIT_RELAY="wss://relay.ngit.dev"
NGIT_PUBKEY="9bd1dde61a8a47964cd1ffe5c8c0814a81582c79e38481d5ce6a894bfc1da60b"
WORKDIR="/tmp/blossom-ci-nip34"
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
for cmd in find fusermount3; do
  command -v "$cmd" >/dev/null 2>&1 || fail "$cmd not found"
done
[ -x "$BLOSSOMFS_BIN" ] || fail "BlossomFS binary not found at $BLOSSOMFS_BIN"

rm -rf "$WORKDIR"
mkdir -p "$WORKDIR" "$MOUNTPOINT"

echo ""
echo "━━━ Scenario 1: Mount NIP-34 git repos via ngit relay ━━━"
info "Pubkey:  ${NGIT_PUBKEY}"
info "Relay:   ${NGIT_RELAY}"

"$BLOSSOMFS_BIN" mount \
  --nip34-relay "$NGIT_RELAY" \
  --nip34-pubkey "$NGIT_PUBKEY" \
  --mountpoint "$MOUNTPOINT" \
  --read-only=true \
  --cache-dir "${WORKDIR}/cache" &
BFS_PID=$!
sleep 8

kill -0 "$BFS_PID" 2>/dev/null || fail "BlossomFS failed to mount"

for i in $(seq 1 20); do
  [ -f "${MOUNTPOINT}/STATUS.txt" ] && break
  sleep 1
done
[ -f "${MOUNTPOINT}/STATUS.txt" ] || fail "STATUS.txt not found (relay query timeout)"

pass "BlossomFS mounted NIP-34 git repos"

echo ""
echo "━━━ Scenario 2: /git/ directory exists ━━━"

GIT_DIR="${MOUNTPOINT}/git"
[ -d "$GIT_DIR" ] || fail "/git/ directory not found"
pass "/git/ directory exists"

PK_DIR="${GIT_DIR}/${NGIT_PUBKEY}"
[ -d "$PK_DIR" ] || fail "/git/<pubkey>/ directory not found"
pass "/git/<pubkey>/ directory exists"

REPO_COUNT=$(find "$PK_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l)
info "Found ${REPO_COUNT} repos"
[ "$REPO_COUNT" -gt 0 ] || fail "No repos found under /git/<pubkey>/"
pass "${REPO_COUNT} repos browseable"

echo ""
echo "━━━ Scenario 3: Repo INFO.md files contain metadata ━━━"

INFO_COUNT=$(find "$PK_DIR" -name "INFO.md" -type f | wc -l)
info "Found ${INFO_COUNT} INFO.md files"
[ "$INFO_COUNT" -gt 0 ] || fail "No INFO.md files found"

FIRST_INFO=$(find "$PK_DIR" -name "INFO.md" -type f | head -1)
REPO_NAME=$(basename $(dirname "$FIRST_INFO"))
info "Sample repo: $REPO_NAME"

if grep -q "Repository ID" "$FIRST_INFO"; then
  pass "INFO.md contains repo metadata"
else
  fail "INFO.md missing expected metadata"
fi

echo ""
echo "━━━ Scenario 4: CLONE_URLS.txt files are readable ━━━"

CLONE_COUNT=$(find "$PK_DIR" -name "CLONE_URLS.txt" -type f | wc -l)
info "Found ${CLONE_COUNT} CLONE_URLS.txt files"
if [ "$CLONE_COUNT" -gt 0 ]; then
  pass "Clone URLs available for ${CLONE_COUNT} repos"
else
  info "No CLONE_URLS.txt (repos may not have clone tags)"
fi

echo ""
echo "━━━ Scenario 5: Issues directory (if any issues exist) ━━━"

TOTAL_ISSUES=$(find "$PK_DIR" -path "*/issues/*.md" -type f | wc -l)
if [ "$TOTAL_ISSUES" -gt 0 ]; then
  info "Found ${TOTAL_ISSUES} issue files"
  FIRST_ISSUE=$(find "$PK_DIR" -path "*/issues/*.md" -type f | head -1)
  ISSUE_REPO=$(basename $(dirname $(dirname "$ISSUE_FILE")))
  info "Sample issue in repo: $(basename $(dirname $(dirname "$FIRST_ISSUE")))"

  if grep -q "#" "$FIRST_ISSUE"; then
    pass "Issue files contain markdown content"
  else
    fail "Issue file appears empty"
  fi
else
  info "No issues found (may not have issues published yet)"
fi

echo ""
echo "━━━ Scenario 6: Patches directory (if any patches exist) ━━━"

TOTAL_PATCHES=$(find "$PK_DIR" -path "*/patches/*.patch" -type f | wc -l)
if [ "$TOTAL_PATCHES" -gt 0 ]; then
  info "Found ${TOTAL_PATCHES} patch files"
  FIRST_PATCH=$(find "$PK_DIR" -path "*/patches/*.patch" -type f | head -1)

  if head -1 "$FIRST_PATCH" | grep -qiE "(diff|from|subject|\[patch)" 2>/dev/null; then
    pass "Patch files contain git format-patch content"
  else
    info "Patch file found but content format unclear"
  fi
else
  info "No patches found (may not have patches published yet)"
fi

echo ""
echo "━━━ Scenario 7: Total file count summary ━━━"

TOTAL_FILES=$(find "$PK_DIR" -type f | wc -l)
info "Total files under /git/<pubkey>/: ${TOTAL_FILES}"
[ "$TOTAL_FILES" -gt 0 ] || fail "No files found at all"
pass "NIP-34 git browser populated with ${TOTAL_FILES} files"

info "Unmounting..."
kill "$BFS_PID" 2>/dev/null
wait "$BFS_PID" 2>/dev/null || true
fusermount3 -u "$MOUNTPOINT" 2>/dev/null || true

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GREEN}NIP-34 CI TEST PASSED${NC}"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Pubkey:       ${NGIT_PUBKEY:0:32}..."
echo "Relay:        $NGIT_RELAY"
echo "Repos:        $REPO_COUNT"
echo "INFO.md:      $INFO_COUNT"
echo "Clone URLs:   $CLONE_COUNT"
echo "Issues:       $TOTAL_ISSUES"
echo "Patches:      $TOTAL_PATCHES"
echo "Total files:  $TOTAL_FILES"
echo ""
echo "BlossomFS successfully browsed NIP-34 git repositories as a filesystem."
