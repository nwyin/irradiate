#!/usr/bin/env bash
# bench/setup.sh — One-time environment setup for benchmarks.
# Run from the project root: bash bench/setup.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MUTMUT_VERSION="${MUTMUT_VERSION:-3.5.0}"
# Portable Python: prefer python3.12 but fall back to python3
BENCH_PYTHON="${BENCH_PYTHON:-$(command -v python3.12 2>/dev/null || command -v python3)}"
cd "$ROOT"

echo "=== irradiate benchmark setup ==="
echo "Project root: $ROOT"
echo

# ── 1. Build irradiate release binary ─────────────────────────────────────
echo "[1/7] Building irradiate (release)..."
cargo build --release
echo "      binary: target/release/irradiate"
echo

# ── 2. simple_project venv ────────────────────────────────────────────────
echo "[2/7] Setting up tests/fixtures/simple_project/.venv..."
cd tests/fixtures/simple_project
if [ ! -d .venv ]; then
    uv venv --python "$BENCH_PYTHON" --seed
fi
uv pip install --python .venv/bin/python pytest
cd "$ROOT"
echo

# ── 3. my_lib venv (optional — vendor/mutmut is gitignored, only present locally)
echo "[3/7] Setting up vendor/mutmut/e2e_projects/my_lib/.venv..."
if [ -d vendor/mutmut/e2e_projects/my_lib ]; then
    cd vendor/mutmut/e2e_projects/my_lib
    if [ ! -d .venv ]; then
        uv venv --python "$BENCH_PYTHON" --seed
    fi
    uv pip install --python .venv/bin/python pytest pytest-asyncio hatchling
    uv pip install --python .venv/bin/python -e .
    cd "$ROOT"
else
    echo "  vendor/mutmut not present (gitignored) — skipping my_lib venv"
fi
echo

# ── 4. synth venv ─────────────────────────────────────────────────────────
echo "[4/7] Setting up bench/targets/synth/.venv..."
cd bench/targets/synth
if [ ! -d .venv ]; then
    uv venv --python "$BENCH_PYTHON" --seed
fi
uv pip install --python .venv/bin/python pytest hatchling
uv pip install --python .venv/bin/python -e .
cd "$ROOT"
echo

# ── 5. mutmut venv (benchmark comparison) ────────────────────────────────
# Default to the current benchmark pin; override with MUTMUT_VERSION if needed.
# This venv also gets the synth package installed so mutmut can run its tests.
echo "[5/7] Setting up bench/.venv (mutmut==$MUTMUT_VERSION for benchmark comparison)..."
if [ ! -d bench/.venv ]; then
    uv venv bench/.venv --python "$BENCH_PYTHON" --seed
fi
uv pip install --python bench/.venv/bin/python "mutmut==$MUTMUT_VERSION" pytest hatchling simplejson
uv pip install --python bench/.venv/bin/python -e bench/targets/synth
# Install benchmark corpora + their test deps into mutmut venv so mutmut can run their tests.
# Each target needs its package installed (editable) plus any test-only deps.
install_into_mutmut() {
    local name="$1"; shift
    if [ -d "bench/corpora/$name" ]; then
        uv pip install --python bench/.venv/bin/python "$@" 2>/dev/null || true
    fi
}
install_into_mutmut markupsafe      -e bench/corpora/markupsafe
install_into_mutmut click           -e "bench/corpora/click[testing]"
install_into_mutmut marshmallow     -e "bench/corpora/marshmallow[tests]"
install_into_mutmut toolz           -e bench/corpora/toolz
install_into_mutmut itsdangerous    freezegun -e bench/corpora/itsdangerous
install_into_mutmut more-itertools  -e bench/corpora/more-itertools

# Inject [tool.mutmut] config into corpora that lack it (corpora are gitignored shallow clones)
inject_mutmut_config() {
    local dir="$1" paths="$2" tests="$3"
    local pyproject="$dir/pyproject.toml"
    # mutmut 3.x requires TOML arrays for paths_to_mutate and tests_dir.
    if [ -f "$pyproject" ] && ! grep -q 'tool.mutmut' "$pyproject"; then
        printf '\n[tool.mutmut]\npaths_to_mutate = ["%s"]\ntests_dir = ["%s"]\n' "$paths" "$tests" >> "$pyproject"
        echo "  Injected [tool.mutmut] into $pyproject"
    fi
}
inject_mutmut_config bench/corpora/marshmallow     "src/marshmallow"    "tests"
inject_mutmut_config bench/corpora/toolz           "toolz"              "toolz/tests"
inject_mutmut_config bench/corpora/markupsafe      "src/markupsafe"     "tests"
inject_mutmut_config bench/corpora/click           "src/click"          "tests"
inject_mutmut_config bench/corpora/itsdangerous    "src/itsdangerous"   "tests"
inject_mutmut_config bench/corpora/more-itertools  "more_itertools"     "tests"

# toolz has tests inside the source package; exclude them from mutation
inject_irradiate_config() {
    local pyproject="$1"
    shift
    if [ -f "$pyproject" ] && ! grep -q 'tool.irradiate' "$pyproject"; then
        printf '\n[tool.irradiate]\n' >> "$pyproject"
        for line in "$@"; do
            printf '%s\n' "$line" >> "$pyproject"
        done
        echo "  Injected [tool.irradiate] into $pyproject"
    fi
}
inject_irradiate_config bench/corpora/toolz/pyproject.toml \
    'do_not_mutate = ["toolz/tests/*", "toolz/sandbox/*"]'
echo

# ── 6. Bootstrap vendor corpora ───────────────────────────────────────────
echo "[6/7] Bootstrapping vendor corpora (bench/corpora/)..."
bash "$ROOT/scripts/bootstrap-vendors.sh"
echo

# ── 7. Set up venvs for vendor corpora ────────────────────────────────────
echo "[7/7] Setting up venvs for vendor corpora..."

setup_vendor_venv() {
    local name="$1"
    shift
    local vdir="$ROOT/bench/corpora/$name"
    if [ ! -d "$vdir" ]; then
        echo "  $name not found (clone may have failed) — skipping venv"
        return 0
    fi
    echo "  Setting up $name venv..."
    (cd "$vdir" && uv venv --python "$BENCH_PYTHON")
    # Install pytest first (always needed), then project deps separately so a
    # build failure in the project doesn't prevent pytest from being available.
    (cd "$vdir" && uv pip install pytest)
    (cd "$vdir" && uv pip install "$@") \
        || echo "  WARNING: $name project install had errors — tests may still work"
}

setup_vendor_venv markupsafe      pytest -e .
setup_vendor_venv click           pytest -e ".[testing]"
setup_vendor_venv httpx           pytest -e .
setup_vendor_venv marshmallow     pytest simplejson -e ".[tests]"
setup_vendor_venv toolz           pytest -e .
setup_vendor_venv itsdangerous    pytest freezegun -e .
setup_vendor_venv more-itertools  pytest -e .
echo

echo "=== Setup complete ==="
echo "Run benchmarks with: bash bench/compare.sh synth"
echo "                 or: bash bench/compare.sh simple_project"
echo "                 or: bash bench/compare.sh my_lib"
echo "Run vendor smoke tests with: bash tests/vendor_test.sh"
