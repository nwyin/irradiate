#!/usr/bin/env bash
# scripts/compat-sweep.sh — Test irradiate against many Python projects.
#
# Usage:
#   bash scripts/compat-sweep.sh                    # run all
#   bash scripts/compat-sweep.sh marshmallow toolz  # run specific projects
#   SAMPLE=20 bash scripts/compat-sweep.sh          # test 20 mutants each
#
# Produces: bench/corpora/compat-report.json and a terminal summary.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/scripts/compat-manifest.json"
CORPORA="$ROOT/bench/corpora"
BINARY="$ROOT/target/release/irradiate"
SAMPLE="${SAMPLE:-5}"
TIMEOUT="${TIMEOUT:-300}"  # 5 minutes per project
REPORT="$CORPORA/compat-report.json"

mkdir -p "$CORPORA"

# ── Require release binary ────────────────────────────────────────────────
if [ ! -x "$BINARY" ]; then
    echo "Building release binary..."
    cargo build --release --manifest-path="$ROOT/Cargo.toml" || exit 1
fi

# ── Parse manifest ────────────────────────────────────────────────────────
# Requires python3 for JSON parsing (avoid jq dependency)
parse_manifest() {
    python3 -c "
import json, sys
with open('$MANIFEST') as f:
    projects = json.load(f)
# Filter to requested projects if any
args = sys.argv[1:]
if args:
    projects = [p for p in projects if p['name'] in args]
for p in projects:
    dnm = '|'.join(p.get('do_not_mutate', []))
    print(f\"{p['name']}\t{p['repo']}\t{p['src']}\t{p['tests']}\t{p['install']}\t{dnm}\")
" "$@"
}

# ── Timeout wrapper ───────────────────────────────────────────────────────
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

# ── Result tracking ──────────────────────────────────────────────────────
declare -a RESULTS=()
PASS=0; FAIL=0; SKIP=0

record() {
    local name="$1" phase="$2" status="$3" detail="$4" mutants="${5:-0}" killed="${6:-0}" survived="${7:-0}"
    RESULTS+=("{\"name\":\"$name\",\"phase\":\"$phase\",\"status\":\"$status\",\"detail\":\"$detail\",\"mutants\":$mutants,\"killed\":$killed,\"survived\":$survived}")
    case "$status" in
        pass) ((PASS++)) ;;
        fail) ((FAIL++)) ;;
        skip) ((SKIP++)) ;;
    esac
}

# ── Per-project test ─────────────────────────────────────────────────────
test_project() {
    local name="$1" repo="$2" src="$3" tests="$4" install="$5" do_not_mutate="$6"
    local pdir="$CORPORA/$name"

    echo ""
    echo "━━━ $name ━━━"

    # Phase 1: Clone
    if [ ! -d "$pdir" ]; then
        echo "  cloning..."
        if ! git clone --depth 1 "$repo" "$pdir" 2>/dev/null; then
            record "$name" "clone" "skip" "clone failed"
            return
        fi
    fi

    # Phase 2: Venv + install
    if [ ! -d "$pdir/.venv" ]; then
        echo "  creating venv..."
        if ! (cd "$pdir" && uv venv --python python3.12 2>/dev/null); then
            record "$name" "venv" "skip" "venv creation failed"
            return
        fi
    fi

    echo "  installing deps..."
    # eval handles shell quoting in install commands (e.g. -e '.[tests]')
    if ! (cd "$pdir" && eval "uv pip install $install" 2>/dev/null); then
        record "$name" "install" "skip" "dep install failed"
        return
    fi

    # Phase 3: Verify tests pass
    echo "  verifying tests..."
    local test_output
    test_output=$(cd "$pdir" && run_with_timeout 120 .venv/bin/python -m pytest "$tests" -q --tb=line -x 2>&1)
    local test_exit=$?
    if [ $test_exit -gt 1 ]; then
        local last_line
        last_line=$(echo "$test_output" | tail -1)
        echo "  tests broken (exit $test_exit): $last_line"
        record "$name" "tests" "skip" "tests broken (exit $test_exit)"
        return
    fi
    local passed
    passed=$(echo "$test_output" | grep -oE '[0-9]+ passed' | grep -oE '[0-9]+' || echo "0")
    echo "  tests: $passed passed"

    # Phase 4: Inject config if needed
    if [ -n "$do_not_mutate" ] && [ -f "$pdir/pyproject.toml" ]; then
        if ! grep -q 'tool.irradiate' "$pdir/pyproject.toml"; then
            local patterns
            patterns=$(echo "$do_not_mutate" | tr '|' '\n' | sed 's/.*/"&"/' | paste -sd ',' -)
            printf '\n[tool.irradiate]\ndo_not_mutate = [%s]\n' "$patterns" >> "$pdir/pyproject.toml"
        fi
    fi

    # Phase 5: Run irradiate
    echo "  running irradiate (--sample $SAMPLE)..."
    rm -rf "$pdir/mutants" "$pdir/.irradiate"
    local irr_output irr_exit
    irr_output=$(cd "$pdir" && run_with_timeout "$TIMEOUT" "$BINARY" run \
        --paths-to-mutate "$src" \
        --tests-dir "$tests" \
        --python .venv/bin/python3 \
        --sample "$SAMPLE" \
        --no-cache \
        --timeout-multiplier 10 2>&1)
    irr_exit=$?

    # Check for panic
    if echo "$irr_output" | grep -q "panicked at"; then
        echo "  PANIC!"
        echo "$irr_output" | grep "panicked at" | head -3
        record "$name" "run" "fail" "panic"
        return
    fi

    # Check for timeout
    if [ $irr_exit -eq 124 ]; then
        echo "  TIMEOUT (${TIMEOUT}s)"
        record "$name" "run" "fail" "timeout"
        return
    fi

    # Parse output
    local mutant_count killed survived score error_line
    mutant_count=$(echo "$irr_output" | grep -oE '[0-9]+ mutants' | head -1 | grep -oE '[0-9]+' || echo "0")
    killed=$(echo "$irr_output" | sed -n 's/.*Killed:[[:space:]]*\([0-9]*\).*/\1/p')
    survived=$(echo "$irr_output" | sed -n 's/.*Survived:[[:space:]]*\([0-9]*\).*/\1/p')
    score=$(echo "$irr_output" | sed -n 's/.*Score:[[:space:]]*\(.*\)/\1/p')
    killed=${killed:-0}
    survived=${survived:-0}

    # Check for known error patterns
    if echo "$irr_output" | grep -qE "^Error:|NameError|ImportError|ModuleNotFoundError"; then
        error_line=$(echo "$irr_output" | grep -E "^Error:|NameError|ImportError|ModuleNotFoundError" | head -1 | cut -c1-120)
        echo "  FAIL: $error_line"
        record "$name" "run" "fail" "$error_line" "$mutant_count" "$killed" "$survived"
        return
    fi

    if [ "$mutant_count" = "0" ]; then
        echo "  no mutants generated"
        record "$name" "run" "fail" "no mutants" "0" "0" "0"
        return
    fi

    echo "  ${mutant_count} mutants, killed=${killed}, survived=${survived}, score=${score}"
    record "$name" "run" "pass" "score=${score}" "$mutant_count" "$killed" "$survived"

    # Cleanup mutants dir (keep .irradiate for debugging)
    rm -rf "$pdir/mutants"
}

# ── Main loop ─────────────────────────────────────────────────────────────
echo "=== irradiate compatibility sweep ==="
echo "Binary:  $BINARY"
echo "Sample:  $SAMPLE mutants per project"
echo "Timeout: ${TIMEOUT}s per project"

while IFS=$'\t' read -r name repo src tests install do_not_mutate; do
    test_project "$name" "$repo" "$src" "$tests" "$install" "$do_not_mutate"
done < <(parse_manifest "$@")

# ── Write JSON report ────────────────────────────────────────────────────
{
    echo "["
    for i in "${!RESULTS[@]}"; do
        if [ "$i" -gt 0 ]; then echo ","; fi
        echo "  ${RESULTS[$i]}"
    done
    echo "]"
} > "$REPORT"

# ── Terminal summary ──────────────────────────────────────────────────────
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Results: $PASS pass, $FAIL fail, $SKIP skip"
echo "Report:  $REPORT"
echo ""

if [ "$FAIL" -gt 0 ]; then
    echo "Failures:"
    python3 -c "
import json
with open('$REPORT') as f:
    for r in json.load(f):
        if r['status'] == 'fail':
            print(f\"  {r['name']:20s} [{r['phase']}] {r['detail']}\")
"
fi
