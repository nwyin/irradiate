#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$ROOT_DIR/target/debug/irradiate"

echo "=== Building irradiate ==="
cargo build --manifest-path="$ROOT_DIR/Cargo.toml"

echo ""
echo "=== E2E: simple_project ==="
FIXTURE="$SCRIPT_DIR/fixtures/simple_project"

# Ensure venv exists
if [ ! -d "$FIXTURE/.venv" ]; then
    echo "Setting up Python venv..."
    (cd "$FIXTURE" && uv venv --python 3.12 && uv pip install pytest)
fi

# Clean previous run
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"

# Run mutation testing (capture output for validation checks)
RUN_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
echo "$RUN_OUTPUT"

# Verify forced-fail validation ran
if ! echo "$RUN_OUTPUT" | grep -q "forced-fail"; then
    echo "FAIL: Expected 'forced-fail' in pipeline output — forced-fail validation may not have run"
    exit 1
fi
echo "  forced-fail validation: OK"

# Verify results
echo ""
echo "--- Checking results ---"
(cd "$FIXTURE" && "$BINARY" results 2>&1)

# Verify .meta file exists
META_FILE="$FIXTURE/mutants/simple_lib/__init__.py.meta"
if [ ! -f "$META_FILE" ]; then
    echo "FAIL: .meta file not found at $META_FILE"
    exit 1
fi
echo "  .meta file exists: OK"

# Parse results and verify counts
RESULTS=$( cd "$FIXTURE" && "$BINARY" results --all 2>&1 )
KILLED=$(echo "$RESULTS" | grep -c "🎉" || true)
SURVIVED=$(echo "$RESULTS" | grep -c "🙁" || true)

echo "  Killed: $KILLED"
echo "  Survived: $SURVIVED"

if [ "$KILLED" -lt 1 ]; then
    echo "FAIL: Expected at least 1 killed mutant, got $KILLED"
    exit 1
fi

if [ "$SURVIVED" -lt 1 ]; then
    echo "FAIL: Expected at least 1 survived mutant, got $SURVIVED"
    exit 1
fi

# Verify show command works
FIRST_KILLED=$(echo "$RESULTS" | grep "🎉" | head -1 | awk '{print $2}')
echo "  Testing 'show' on $FIRST_KILLED..."
SHOW_OUTPUT=$( cd "$FIXTURE" && "$BINARY" show "$FIRST_KILLED" 2>&1 )
if ! echo "$SHOW_OUTPUT" | grep -q "^[+-]"; then
    echo "FAIL: 'show' command didn't produce diff output"
    exit 1
fi
echo "  show command: OK"

# Clean up
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"

echo ""
echo "=== E2E: --isolate flag ==="

# Capture reference killed/survived counts from the standard (worker pool) run
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"
POOL_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
POOL_RESULTS=$( cd "$FIXTURE" && "$BINARY" results --all 2>&1 )
POOL_KILLED=$(echo "$POOL_RESULTS" | grep -c "🎉" || true)
POOL_SURVIVED=$(echo "$POOL_RESULTS" | grep -c "🙁" || true)
echo "  Worker pool: Killed=$POOL_KILLED, Survived=$POOL_SURVIVED"

# Now run with --isolate and compare
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"
ISOLATE_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 --isolate 2>&1 )
echo "$ISOLATE_OUTPUT"

# Verify isolated mode message
if ! echo "$ISOLATE_OUTPUT" | grep -q "isolated mode"; then
    echo "FAIL: Expected 'isolated mode' in --isolate output"
    exit 1
fi
echo "  isolated mode message: OK"

ISOLATE_RESULTS=$( cd "$FIXTURE" && "$BINARY" results --all 2>&1 )
ISOLATE_KILLED=$(echo "$ISOLATE_RESULTS" | grep -c "🎉" || true)
ISOLATE_SURVIVED=$(echo "$ISOLATE_RESULTS" | grep -c "🙁" || true)
echo "  Isolated: Killed=$ISOLATE_KILLED, Survived=$ISOLATE_SURVIVED"

# INV-1: --isolate must produce identical killed/survived counts
if [ "$ISOLATE_KILLED" -ne "$POOL_KILLED" ]; then
    echo "FAIL: --isolate killed count ($ISOLATE_KILLED) differs from worker pool ($POOL_KILLED)"
    exit 1
fi
if [ "$ISOLATE_SURVIVED" -ne "$POOL_SURVIVED" ]; then
    echo "FAIL: --isolate survived count ($ISOLATE_SURVIVED) differs from worker pool ($POOL_SURVIVED)"
    exit 1
fi
echo "  --isolate matches worker pool results: OK"

# Clean up
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"

echo ""
echo "=== E2E tests: PASS ==="
