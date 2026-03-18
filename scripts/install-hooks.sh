#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GIT_COMMON="$(git -C "$ROOT" rev-parse --git-common-dir)"
mkdir -p "$GIT_COMMON/hooks"
ln -sf "$ROOT/scripts/pre-commit" "$GIT_COMMON/hooks/pre-commit"
echo "Pre-commit hook installed."
