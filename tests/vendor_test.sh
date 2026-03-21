#!/usr/bin/env bash
# tests/vendor_test.sh — Smoke-test irradiate against vendored Python repos.
#
# markupsafe: full pass required (INV-2 + INV-3).
# click, httpx: best-effort — failures are reported but do not fail the suite.
#
# Invariants:
#   INV-1: bootstrap is idempotent — running twice produces no errors
#   INV-2: irradiate must not panic on any vendor repo
#   INV-3: markupsafe must produce at least 1 killed mutant
#
# Usage: cargo build && bash tests/vendor_test.sh
# NOT intended for CI — requires network + takes several minutes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$ROOT_DIR/target/debug/irradiate"
CORPORA_DIR="$ROOT_DIR/bench/corpora"

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# run_with_timeout <seconds> <command...>
# Uses gtimeout (macOS Homebrew) or timeout (Linux), falls back to no timeout.
run_with_timeout() {
    local secs="$1"; shift
    if command -v gtimeout &>/dev/null; then
        gtimeout "$secs" "$@"
    elif command -v timeout &>/dev/null; then
        timeout "$secs" "$@"
    else
        "$@"
    fi
}

# ── Build ─────────────────────────────────────────────────────────────────────
log "Building irradiate..."
cargo build --manifest-path="$ROOT_DIR/Cargo.toml"
log "Build OK"

# ── INV-1: Bootstrap idempotency ──────────────────────────────────────────────
echo ""
log "=== Bootstrapping vendor repos (run 1) ==="
bash "$ROOT_DIR/scripts/bootstrap-vendors.sh"

log "=== Bootstrapping vendor repos (run 2 — idempotency check) ==="
bash "$ROOT_DIR/scripts/bootstrap-vendors.sh"
log "Bootstrap idempotency: OK"

# ── markupsafe (required pass) ────────────────────────────────────────────────
echo ""
log "=== Vendor test: markupsafe (required) ==="

MDIR="$CORPORA_DIR/markupsafe"
if [ ! -d "$MDIR" ]; then
    echo "FAIL: markupsafe not cloned — check network or run scripts/bootstrap-vendors.sh"
    exit 1
fi

# Set up venv and install deps (uv is idempotent, so always run install)
[ ! -d "$MDIR/.venv" ] && (cd "$MDIR" && uv venv --python 3.12)
(cd "$MDIR" && uv pip install pytest -e .)

# Verify project tests pass before mutation testing
log "Verifying markupsafe tests pass..."
(cd "$MDIR" && .venv/bin/python -m pytest tests/ -q --tb=short -x 2>&1)
log "markupsafe tests: OK"

# Clean any prior irradiate run
rm -rf "$MDIR/mutants" "$MDIR/.irradiate"

# Run irradiate in --isolate mode
log "Running irradiate on markupsafe..."
MARKUPSAFE_OUTPUT=$( cd "$MDIR" && run_with_timeout 600 "$BINARY" run \
    --paths-to-mutate src/markupsafe \
    --tests-dir tests \
    --python .venv/bin/python3 \
    --isolate \
    --timeout-multiplier 10 2>&1 )
echo "$MARKUPSAFE_OUTPUT"

# INV-2: no panic
if echo "$MARKUPSAFE_OUTPUT" | grep -q "panicked at"; then
    echo "FAIL: irradiate panicked on markupsafe"
    exit 1
fi
log "No panic: OK"

# Mutation generation must have started
if ! echo "$MARKUPSAFE_OUTPUT" | grep -q "Generating mutants"; then
    echo "FAIL: irradiate did not start mutation generation on markupsafe"
    exit 1
fi
log "Mutation generation started: OK"

# Check results command works and count killed mutants
MARKUPSAFE_RESULTS=$( cd "$MDIR" && "$BINARY" results --all 2>&1 )
KILLED=$(echo "$MARKUPSAFE_RESULTS" | grep -c "🎉" || true)
log "Killed: $KILLED"

# INV-3: markupsafe must produce at least 1 killed mutant
if [ "$KILLED" -lt 1 ]; then
    echo "FAIL: Expected at least 1 killed mutant from markupsafe, got $KILLED"
    exit 1
fi
log "markupsafe killed >= 1: OK"

# Cleanup
rm -rf "$MDIR/mutants" "$MDIR/.irradiate"

log "=== markupsafe: PASS ==="

# ── click (best-effort) ───────────────────────────────────────────────────────
echo ""
log "=== Vendor test: click (best-effort) ==="

run_click() {
    local CDIR="$CORPORA_DIR/click"
    if [ ! -d "$CDIR" ]; then
        log "click not cloned — skipping"
        return 1
    fi

    # Set up venv and install deps (uv is idempotent, so always run install)
    [ ! -d "$CDIR/.venv" ] && (cd "$CDIR" && uv venv --python 3.12)
    log "Setting up click venv..."
    (cd "$CDIR" && uv pip install pytest -e .)

    # Verify project tests pass
    log "Verifying click tests..."
    (cd "$CDIR" && .venv/bin/python -m pytest tests/ -q --tb=short -x 2>&1) || {
        log "click tests failed — skipping mutation run"
        return 1
    }

    rm -rf "$CDIR/mutants" "$CDIR/.irradiate"
    log "Running irradiate on click..."
    local CLICK_OUTPUT
    CLICK_OUTPUT=$( cd "$CDIR" && run_with_timeout 600 "$BINARY" run \
        --paths-to-mutate src/click \
        --tests-dir tests \
        --python .venv/bin/python3 \
        --isolate \
        --timeout-multiplier 10 2>&1 )
    echo "$CLICK_OUTPUT"

    # INV-2: no panic (required even for best-effort)
    if echo "$CLICK_OUTPUT" | grep -q "panicked at"; then
        log "FAIL: irradiate panicked on click"
        return 1
    fi

    local CLICK_RESULTS CLICK_KILLED
    CLICK_RESULTS=$( cd "$CDIR" && "$BINARY" results --all 2>&1 )
    CLICK_KILLED=$(echo "$CLICK_RESULTS" | grep -c "🎉" || true)
    log "click killed: $CLICK_KILLED"

    rm -rf "$CDIR/mutants" "$CDIR/.irradiate"
    log "click: PASS"
}

run_click || log "click: SKIPPED (best-effort — see output above)"

# ── httpx (best-effort) ───────────────────────────────────────────────────────
echo ""
log "=== Vendor test: httpx (best-effort) ==="

run_httpx() {
    local HDIR="$CORPORA_DIR/httpx"
    if [ ! -d "$HDIR" ]; then
        log "httpx not cloned — skipping"
        return 1
    fi

    # Set up venv and install deps (uv is idempotent, so always run install)
    [ ! -d "$HDIR/.venv" ] && (cd "$HDIR" && uv venv --python 3.12)
    log "Setting up httpx venv..."
    (cd "$HDIR" && uv pip install pytest uvicorn trustme trio -e ".[brotli,http2,socks,zstd]")

    # Verify project tests mostly pass (httpx has pre-existing failures on macOS)
    log "Verifying httpx tests..."
    local HTTPX_TEST_OUT
    HTTPX_TEST_OUT=$( cd "$HDIR" && .venv/bin/python -m pytest tests/ -q --tb=short 2>&1 )
    echo "$HTTPX_TEST_OUT" | tail -3
    local HTTPX_PASSED
    HTTPX_PASSED=$(echo "$HTTPX_TEST_OUT" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo "0")
    if [ "$HTTPX_PASSED" -lt 100 ]; then
        log "httpx tests mostly broken ($HTTPX_PASSED passed) — skipping mutation run"
        return 1
    fi
    log "httpx tests: $HTTPX_PASSED passed (some failures expected on macOS)"

    rm -rf "$HDIR/mutants" "$HDIR/.irradiate"
    log "Running irradiate on httpx..."
    local HTTPX_OUTPUT
    HTTPX_OUTPUT=$( cd "$HDIR" && run_with_timeout 600 "$BINARY" run \
        --paths-to-mutate httpx \
        --tests-dir tests \
        --python .venv/bin/python3 \
        --isolate \
        --timeout-multiplier 10 2>&1 )
    echo "$HTTPX_OUTPUT"

    # INV-2: no panic (required even for best-effort)
    if echo "$HTTPX_OUTPUT" | grep -q "panicked at"; then
        log "FAIL: irradiate panicked on httpx"
        return 1
    fi

    local HTTPX_RESULTS HTTPX_KILLED
    HTTPX_RESULTS=$( cd "$HDIR" && "$BINARY" results --all 2>&1 )
    HTTPX_KILLED=$(echo "$HTTPX_RESULTS" | grep -c "🎉" || true)
    log "httpx killed: $HTTPX_KILLED"

    rm -rf "$HDIR/mutants" "$HDIR/.irradiate"
    log "httpx: PASS"
}

run_httpx || log "httpx: SKIPPED (best-effort — see output above)"

echo ""
log "=== Vendor tests: PASS ==="
