#!/usr/bin/env bash
# scripts/bootstrap-vendors.sh — Shallow-clone vendor Python repos into bench/corpora/.
# Idempotent: skips repos that are already present, cleans up partial clones on failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
CORPORA_DIR="$ROOT_DIR/bench/corpora"

mkdir -p "$CORPORA_DIR"

clone_if_missing() {
    local name="$1"
    local url="$2"
    local dest="$CORPORA_DIR/$name"
    if [ -d "$dest" ]; then
        echo "  $name already present, skipping"
        return 0
    fi
    echo "  Cloning $name..."
    if git clone --depth 1 "$url" "$dest" 2>&1; then
        echo "  $name: cloned OK"
    else
        echo "  WARNING: Failed to clone $name (network unavailable?) — skipping"
        rm -rf "$dest"
        return 1
    fi
}

echo "=== Bootstrapping vendor corpora ==="
clone_if_missing markupsafe  "https://github.com/pallets/markupsafe"          || true
clone_if_missing click       "https://github.com/pallets/click"               || true
clone_if_missing httpx       "https://github.com/encode/httpx"                || true
clone_if_missing marshmallow "https://github.com/marshmallow-code/marshmallow" || true
clone_if_missing toolz          "https://github.com/pytoolz/toolz"               || true
clone_if_missing itsdangerous   "https://github.com/pallets/itsdangerous"        || true
clone_if_missing more-itertools "https://github.com/more-itertools/more-itertools" || true
echo "=== Done ==="
