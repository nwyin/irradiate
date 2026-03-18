#!/usr/bin/env bash
# bench/setup.sh — One-time environment setup for benchmarks.
# Run from the project root: bash bench/setup.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "=== irradiate benchmark setup ==="
echo "Project root: $ROOT"
echo

# ── 1. Build irradiate release binary ─────────────────────────────────────
echo "[1/4] Building irradiate (release)..."
cargo build --release
echo "      binary: target/release/irradiate"
echo

# ── 2. Bench venv with mutmut installed ───────────────────────────────────
echo "[2/4] Setting up bench/.venv with mutmut..."
uv venv bench/.venv --python python3.12 --seed
uv pip install --python bench/.venv/bin/python \
    -e vendor/mutmut \
    pytest \
    pytest-asyncio
echo "      mutmut: $(bench/.venv/bin/mutmut version 2>/dev/null || echo 'installed')"
echo

# ── 3. simple_project venv ────────────────────────────────────────────────
echo "[3/4] Setting up tests/fixtures/simple_project/.venv..."
cd tests/fixtures/simple_project
if [ ! -d .venv ]; then
    uv venv --python python3.12 --seed
fi
uv pip install --python .venv/bin/python pytest
cd "$ROOT"
echo

# ── 4. my_lib venv ────────────────────────────────────────────────────────
echo "[4/4] Setting up vendor/mutmut/e2e_projects/my_lib/.venv..."
cd vendor/mutmut/e2e_projects/my_lib
if [ ! -d .venv ]; then
    uv venv --python python3.12 --seed
fi
uv pip install --python .venv/bin/python pytest pytest-asyncio hatchling
uv pip install --python .venv/bin/python -e .
cd "$ROOT"
echo

echo "=== Setup complete ==="
echo "Run benchmarks with: bash bench/compare.sh simple_project"
echo "                 or: bash bench/compare.sh my_lib"
