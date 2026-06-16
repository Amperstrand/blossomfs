#!/usr/bin/env bash
#
# mount-blossomfs.sh — convenience wrapper for mounting BlossomFS
#
# Usage:
#   ./mount-blossomfs.sh mount <mountpoint> [options...]
#   ./mount-blossomfs.sh unmount <mountpoint>
#   ./mount-blossomfs.sh status
#
# Examples:
#   ./mount-blossomfs.sh mount /mnt/blossom --config ~/.config/blossomfs.toml
#   ./mount-blossomfs.sh mount /mnt/blossom --npub npub1... --server https://blossom.example.com
#   ./mount-blossomfs.sh unmount /mnt/blossom
#   ./mount-blossomfs.sh status
#
set -euo pipefail

BINARY="${BLOSSOMFS_BINARY:-./target/release/blossomfs}"

if [ ! -f "$BINARY" ]; then
    echo "error: blossomfs binary not found at $BINARY"
    echo "       build it first: cargo build --release"
    exit 1
fi

ACTION="${1:-}"
MOUNTPOINT="${2:-}"

case "$ACTION" in
    mount)
        if [ -z "$MOUNTPOINT" ]; then
            echo "usage: $0 mount <mountpoint> [options...]"
            exit 1
        fi
        mkdir -p "$MOUNTPOINT"
        shift 2
        exec "$BINARY" mount --mountpoint "$MOUNTPOINT" "$@"
        ;;
    unmount)
        if [ -z "$MOUNTPOINT" ]; then
            echo "usage: $0 unmount <mountpoint>"
            exit 1
        fi
        fusermount3 -u "$MOUNTPOINT" 2>/dev/null || sudo umount "$MOUNTPOINT"
        echo "unmounted $MOUNTPOINT"
        ;;
    status)
        mount | grep blossomfs || echo "no blossomfs mounts found"
        ;;
    *)
        echo "usage: $0 {mount|unmount|status} <mountpoint> [options...]"
        exit 1
        ;;
esac
