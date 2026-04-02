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

# ── 5. synth venv extras ─────────────────────────────────────────────────
# The synth target needs mutmut installed in its own venv for comparison.
echo "[5/7] Installing mutmut into synth venv..."
uv pip install --python bench/targets/synth/.venv/bin/python "mutmut==$MUTMUT_VERSION" 2>/dev/null || true

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
    # Install pytest + mutmut first (always needed), then project deps separately
    # so a build failure in the project doesn't prevent pytest from being available.
    (cd "$vdir" && uv pip install pytest "mutmut==$MUTMUT_VERSION")
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

# ── 8. Inject tool configs into corpora ──────────────────────────────────
# Must happen after bootstrap (step 6) so the pyproject.toml files exist.

inject_mutmut_config() {
    local dir="$1" paths="$2" tests="$3"
    shift 3
    local pyproject="$dir/pyproject.toml"
    if [ -f "$pyproject" ] && ! grep -q 'tool.mutmut' "$pyproject"; then
        printf '\n[tool.mutmut]\npaths_to_mutate = ["%s"]\ntests_dir = ["%s"]\n' "$paths" "$tests" >> "$pyproject"
        for line in "$@"; do
            printf '%s\n' "$line" >> "$pyproject"
        done
        echo "  Injected [tool.mutmut] into $pyproject"
    fi
}
inject_mutmut_config bench/corpora/marshmallow     "src/marshmallow"    "tests"
inject_mutmut_config bench/corpora/toolz           "toolz"              "toolz/tests" \
    'pytest_add_cli_args_test_selection = ["-k", "not test_curried_operator"]'
inject_mutmut_config bench/corpora/markupsafe      "src/markupsafe"     "tests"
inject_mutmut_config bench/corpora/click           "src/click"          "tests" \
    'pytest_add_cli_args_test_selection = ["-k", "not test_global_context_object"]'
inject_mutmut_config bench/corpora/itsdangerous    "src/itsdangerous"   "tests"

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

echo "=== Setup complete ==="
echo "Run benchmarks with: bash bench/compare.sh synth"
echo "                 or: bash bench/compare.sh simple_project"
echo "                 or: bash bench/compare.sh my_lib"
echo "Run vendor smoke tests with: bash tests/vendor_test.sh"
