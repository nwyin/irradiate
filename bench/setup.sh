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
echo "[1/7] Building irradiate (release)..."
cargo build --release
echo "      binary: target/release/irradiate"
echo

# ── 2. Bench venv with mutmut installed ───────────────────────────────────
echo "[2/7] Setting up bench/.venv with mutmut..."
uv venv bench/.venv --python python3.12 --seed
uv pip install --python bench/.venv/bin/python \
    -e vendor/mutmut \
    pytest \
    pytest-asyncio
echo "      mutmut: $(bench/.venv/bin/mutmut version 2>/dev/null || echo 'installed')"
echo

# ── 3. simple_project venv ────────────────────────────────────────────────
echo "[3/7] Setting up tests/fixtures/simple_project/.venv..."
cd tests/fixtures/simple_project
if [ ! -d .venv ]; then
    uv venv --python python3.12 --seed
fi
uv pip install --python .venv/bin/python pytest
cd "$ROOT"
echo

# ── 4. my_lib venv ────────────────────────────────────────────────────────
echo "[4/7] Setting up vendor/mutmut/e2e_projects/my_lib/.venv..."
cd vendor/mutmut/e2e_projects/my_lib
if [ ! -d .venv ]; then
    uv venv --python python3.12 --seed
fi
uv pip install --python .venv/bin/python pytest pytest-asyncio hatchling
uv pip install --python .venv/bin/python -e .
cd "$ROOT"
echo

# ── 5. synth venv ─────────────────────────────────────────────────────────
echo "[5/7] Setting up bench/targets/synth/.venv..."
cd bench/targets/synth
if [ ! -d .venv ]; then
    uv venv --python python3.12 --seed
fi
uv pip install --python .venv/bin/python pytest hatchling
uv pip install --python .venv/bin/python -e .
cd "$ROOT"
echo

# ── 6. Bootstrap vendor corpora ───────────────────────────────────────────
echo "[6/7] Bootstrapping vendor corpora (bench/corpora/)..."
bash "$ROOT/scripts/bootstrap-vendors.sh"
echo

# ── 7. Set up venvs for vendor corpora ────────────────────────────────────
echo "[7/7] Setting up venvs for vendor corpora..."

setup_vendor_venv() {
    local name="$1"
    local install_args="$2"
    local vdir="$ROOT/bench/corpora/$name"
    if [ ! -d "$vdir" ]; then
        echo "  $name not found (clone may have failed) — skipping venv"
        return 0
    fi
    if [ ! -d "$vdir/.venv" ]; then
        echo "  Setting up $name venv..."
        (cd "$vdir" && uv venv --python python3.12)
        # shellcheck disable=SC2086
        (cd "$vdir" && uv pip install $install_args) \
            || echo "  WARNING: $name install failed — venv may be incomplete"
    else
        echo "  $name .venv already present, skipping"
    fi
}

setup_vendor_venv markupsafe "pytest -e ."
setup_vendor_venv click      "pytest -e '.[testing]'"
setup_vendor_venv httpx      "pytest -e ."
echo

echo "=== Setup complete ==="
echo "Run benchmarks with: bash bench/compare.sh simple_project"
echo "                 or: bash bench/compare.sh my_lib"
echo "                 or: bash bench/compare.sh synth"
echo "Run vendor smoke tests with: bash tests/vendor_test.sh"
