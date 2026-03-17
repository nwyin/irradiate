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

# Run mutation testing
(cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1)

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
echo "=== E2E tests: PASS ==="
