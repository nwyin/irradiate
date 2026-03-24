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

# Verify stats + validation ran (consolidates clean + fail into one subprocess)
if ! echo "$RUN_OUTPUT" | grep -q "stats + validation"; then
    echo "FAIL: Expected 'stats + validation' in pipeline output"
    exit 1
fi
echo "  stats + validation: OK"

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

# Verify cache warms on a second identical run
echo ""
echo "--- Checking cache warm run ---"
CACHE_WARM_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
echo "$CACHE_WARM_OUTPUT"
CACHE_HITS=$(echo "$CACHE_WARM_OUTPUT" | grep -oE 'Cache hits: [0-9]+' | awk '{print $3}' | tail -1)
if [ -z "${CACHE_HITS:-}" ] || [ "$CACHE_HITS" -le 0 ]; then
    echo "FAIL: Expected warm run to report cache hits, got '${CACHE_HITS:-missing}'"
    exit 1
fi
echo "  warm cache hits: OK ($CACHE_HITS)"

# Verify changing a selected test file invalidates the cache
echo ""
echo "--- Checking cache invalidation on test file change ---"
TEST_FILE="$FIXTURE/tests/test_simple.py"
TEST_FILE_BAK="$(mktemp)"
cp "$TEST_FILE" "$TEST_FILE_BAK"
trap 'cp "$TEST_FILE_BAK" "$TEST_FILE" 2>/dev/null || true; rm -f "$TEST_FILE_BAK"' EXIT
printf '\n# cache invalidation marker\n' >> "$TEST_FILE"
INVALIDATE_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
echo "$INVALIDATE_OUTPUT"
INVALIDATE_HITS=$(echo "$INVALIDATE_OUTPUT" | grep -oE 'Cache hits: [0-9]+' | awk '{print $3}' | tail -1)
if [ -z "${INVALIDATE_HITS:-}" ] || [ "$INVALIDATE_HITS" -ne 0 ]; then
    echo "FAIL: Expected test file change to invalidate cache hits, got '${INVALIDATE_HITS:-missing}'"
    exit 1
fi
cp "$TEST_FILE_BAK" "$TEST_FILE"
rm -f "$TEST_FILE_BAK"
trap - EXIT
echo "  cache invalidation on test change: OK"

# Verify cache clean removes only cache state
echo ""
echo "--- Checking cache clean ---"
( cd "$FIXTURE" && "$BINARY" cache clean 2>&1 )
if [ -d "$FIXTURE/.irradiate/cache" ]; then
    echo "FAIL: Expected cache clean to remove $FIXTURE/.irradiate/cache"
    exit 1
fi
CACHE_COLD_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
echo "$CACHE_COLD_OUTPUT"
CACHE_COLD_HITS=$(echo "$CACHE_COLD_OUTPUT" | grep -oE 'Cache hits: [0-9]+' | awk '{print $3}' | tail -1)
if [ -z "${CACHE_COLD_HITS:-}" ] || [ "$CACHE_COLD_HITS" -ne 0 ]; then
    echo "FAIL: Expected run after cache clean to be cold, got '${CACHE_COLD_HITS:-missing}' cache hits"
    exit 1
fi
echo "  cache clean: OK"

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
echo "=== Harness self-mutation ==="
HARNESS_TESTS_DIR="$SCRIPT_DIR/harness_tests"

# Set up harness test venv if needed
if [ ! -d "$HARNESS_TESTS_DIR/.venv" ]; then
    echo "Setting up harness test venv..."
    (cd "$HARNESS_TESTS_DIR" && uv venv --python 3.12 && uv pip install pytest)
fi

# Verify the harness tests pass on their own before mutation testing
echo "  Running harness unit tests..."
(cd "$HARNESS_TESTS_DIR" && .venv/bin/python -m pytest -q 2>&1)
echo "  Harness unit tests: OK"

# Run irradiate on its own harness (verify it runs without crashing)
# mutants/ and .irradiate/ are created relative to cwd (ROOT_DIR)
rm -rf "$ROOT_DIR/mutants" "$ROOT_DIR/.irradiate"

echo "  Running irradiate on harness/ (self-mutation)..."
SELFMUT_OUTPUT=$( cd "$ROOT_DIR" && \
    "$BINARY" run \
    --paths-to-mutate harness/ \
    --tests-dir "$HARNESS_TESTS_DIR" \
    --python "$HARNESS_TESTS_DIR/.venv/bin/python3" \
    --timeout-multiplier 10 \
    --isolate 2>&1 || true )
echo "$SELFMUT_OUTPUT"

# Verify it completed without a panic (exit 0 or non-zero both acceptable; panic = "panicked at")
if echo "$SELFMUT_OUTPUT" | grep -q "panicked at"; then
    echo "FAIL: irradiate panicked during harness self-mutation"
    exit 1
fi
echo "  Harness self-mutation: OK (no panic)"

# Clean up
rm -rf "$ROOT_DIR/mutants" "$ROOT_DIR/.irradiate"

echo ""
echo "=== E2E: --diff incremental mode ==="

INCR_TMPDIR="$(mktemp -d /tmp/irradiate_incr_XXXXXX)"
cleanup_incr() { rm -rf "$INCR_TMPDIR"; }
trap cleanup_incr EXIT

# Create project structure
mkdir -p "$INCR_TMPDIR/src/math_ops" "$INCR_TMPDIR/tests"

cat > "$INCR_TMPDIR/src/math_ops/__init__.py" << 'PYEOF'
def add(a, b):
    return a + b


def multiply(a, b):
    return a * b
PYEOF

cat > "$INCR_TMPDIR/tests/test_math.py" << 'PYEOF'
from math_ops import add, multiply


def test_add():
    assert add(2, 3) == 5
    assert add(0, 0) == 0


def test_multiply():
    assert multiply(2, 3) == 6
    assert multiply(0, 5) == 0
PYEOF

cat > "$INCR_TMPDIR/pyproject.toml" << 'PYEOF'
[build-system]
requires = ["setuptools"]
build-backend = "setuptools.backends.legacy:build"

[project]
name = "math_ops"
version = "0.1.0"
PYEOF

# Set up Python venv
(cd "$INCR_TMPDIR" && uv venv --python 3.12 -q && uv pip install pytest -q)

# Initialize git repo and make initial commit
git -C "$INCR_TMPDIR" init -q
git -C "$INCR_TMPDIR" config user.email "test@example.com"
git -C "$INCR_TMPDIR" config user.name "Test"
git -C "$INCR_TMPDIR" add .
git -C "$INCR_TMPDIR" commit -qm "initial"
INITIAL_BRANCH=$(git -C "$INCR_TMPDIR" rev-parse --abbrev-ref HEAD)

# Run full mutation test for comparison baseline (on initial commit)
FULL_OUTPUT=$( cd "$INCR_TMPDIR" && \
    "$BINARY" run \
    --paths-to-mutate src \
    --tests-dir tests \
    --python .venv/bin/python3 2>&1 )
echo "$FULL_OUTPUT"
FULL_RESULTS=$( cd "$INCR_TMPDIR" && "$BINARY" results --all 2>&1 )
FULL_TOTAL=$(echo "$FULL_RESULTS" | grep -cE "🎉|🙁" || true)
echo "  Full run (initial) total mutants: $FULL_TOTAL"

if [ "$FULL_TOTAL" -lt 1 ]; then
    echo "FAIL: Full run produced no mutants"
    exit 1
fi

# Create branch, modify add() to have more operators (keeps tests passing)
git -C "$INCR_TMPDIR" checkout -qb feature
cat > "$INCR_TMPDIR/src/math_ops/__init__.py" << 'PYEOF'
def add(a, b):
    result = a + b
    check = result > 0
    return result


def multiply(a, b):
    return a * b
PYEOF
git -C "$INCR_TMPDIR" commit -qam "modify add"

# Run incremental mutation test (--diff <initial_branch>)
rm -rf "$INCR_TMPDIR/mutants" "$INCR_TMPDIR/.irradiate"
INCR_OUTPUT=$( cd "$INCR_TMPDIR" && \
    "$BINARY" run \
    --paths-to-mutate src \
    --tests-dir tests \
    --python .venv/bin/python3 \
    --diff "$INITIAL_BRANCH" 2>&1 )
echo "$INCR_OUTPUT"
INCR_RESULTS=$( cd "$INCR_TMPDIR" && "$BINARY" results --all 2>&1 )
INCR_TOTAL=$(echo "$INCR_RESULTS" | grep -cE "🎉|🙁" || true)
echo "  Incremental run total mutants: $INCR_TOTAL"

# Also run a full mutation test on the modified branch for comparison
rm -rf "$INCR_TMPDIR/mutants" "$INCR_TMPDIR/.irradiate"
MODIFIED_FULL_OUTPUT=$( cd "$INCR_TMPDIR" && \
    "$BINARY" run \
    --paths-to-mutate src \
    --tests-dir tests \
    --python .venv/bin/python3 2>&1 )
MODIFIED_FULL_RESULTS=$( cd "$INCR_TMPDIR" && "$BINARY" results --all 2>&1 )
MODIFIED_FULL_TOTAL=$(echo "$MODIFIED_FULL_RESULTS" | grep -cE "🎉|🙁" || true)
echo "  Full run (modified branch) total mutants: $MODIFIED_FULL_TOTAL"

# INV-1: Incremental run produces fewer mutants than full run on same branch
if [ "$INCR_TOTAL" -ge "$MODIFIED_FULL_TOTAL" ]; then
    echo "FAIL: INV-1: Incremental run ($INCR_TOTAL mutants) should be less than full run ($MODIFIED_FULL_TOTAL mutants)"
    exit 1
fi
echo "  INV-1: incremental < full: OK ($INCR_TOTAL < $MODIFIED_FULL_TOTAL)"

# INV-2: No multiply() mutants in incremental run (multiply was not changed)
if echo "$INCR_RESULTS" | grep -q "multiply"; then
    echo "FAIL: INV-2: Incremental run produced mutants for multiply() which was not changed"
    exit 1
fi
echo "  INV-2: no multiply() mutants in incremental run: OK"

# INV-2 (cont): add() mutants must be present in incremental run
if ! echo "$INCR_RESULTS" | grep -q "add"; then
    echo "FAIL: INV-2: Incremental run produced no mutants for add() which was changed"
    exit 1
fi
echo "  INV-2: add() mutants present in incremental run: OK"

trap - EXIT
rm -rf "$INCR_TMPDIR"

# === --sample flag ===
echo ""
echo "=== E2E: --sample flag ==="
rm -rf "$FIXTURE/mutants" "$FIXTURE/.irradiate"

SAMPLE_OUTPUT=$( cd "$FIXTURE" && "$BINARY" run --python .venv/bin/python3 --sample 0.5 2>&1 )
echo "$SAMPLE_OUTPUT"

if ! echo "$SAMPLE_OUTPUT" | grep -q "Sampled"; then
    echo "FAIL: Expected 'Sampled' in --sample output"
    exit 1
fi
echo "  --sample output contains 'Sampled': OK"

if ! echo "$SAMPLE_OUTPUT" | grep -q "sampled"; then
    echo "FAIL: Expected 'sampled' note in score output"
    exit 1
fi
echo "  --sample score note present: OK"

# ── Regex mutation detection ──
echo ""
echo "=== E2E: regex_project ==="
REGEX_FIXTURE="$SCRIPT_DIR/fixtures/regex_project"

# Ensure venv exists
if [ ! -d "$REGEX_FIXTURE/.venv" ]; then
    echo "Setting up Python venv..."
    (cd "$REGEX_FIXTURE" && uv venv --python 3.12 && uv pip install pytest)
fi

# Clean previous run
rm -rf "$REGEX_FIXTURE/mutants" "$REGEX_FIXTURE/.irradiate"

# Run mutation testing
REGEX_OUTPUT=$( cd "$REGEX_FIXTURE" && "$BINARY" run --python .venv/bin/python3 2>&1 )
echo "$REGEX_OUTPUT"

# Verify pipeline completed
if ! echo "$REGEX_OUTPUT" | grep -q "stats + validation"; then
    echo "FAIL: regex_project did not complete stats + validation"
    exit 1
fi
echo "  regex_project completed: OK"

# Verify mutations were found (at least some killed)
REGEX_RESULTS=$( cd "$REGEX_FIXTURE" && "$BINARY" results --all 2>&1 )
REGEX_KILLED=$(echo "$REGEX_RESULTS" | grep -c "🎉" || true)
echo "  Regex killed: $REGEX_KILLED"

if [ "$REGEX_KILLED" -lt 1 ]; then
    echo "FAIL: Expected at least 1 killed mutant in regex_project, got $REGEX_KILLED"
    exit 1
fi
echo "  regex_project killed mutants: OK"

echo ""
echo "=== E2E tests: PASS ==="
