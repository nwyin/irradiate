#!/usr/bin/env bash
# bench/compare.sh — Run irradiate benchmarks for a given target.
#
# Usage:
#   bash bench/compare.sh <target_name> [--runs N]
#
# Examples:
#   bash bench/compare.sh simple_project
#   bash bench/compare.sh my_lib --runs 5
#
# Environment overrides:
#   BENCH_RUNS=N    number of timed runs (default: 3)
#
# ── APPLES-TO-ORANGES NOTE ──────────────────────────────────────────────────
# This script compares irradiate against mutmut 2.5.1. These tools use
# fundamentally different mutation architectures:
#
#   irradiate   — trampoline-based: all mutant variants compiled into one file,
#                 switching via a global variable (no per-mutant disk I/O).
#                 Parsing via libcst (Rust-native, pyo3).
#
#   mutmut 2.x  — disk-based: writes mutated source to disk, runs pytest,
#                 restores original after each mutant. Each mutant involves
#                 file I/O. Parsing via parso (pure Python AST).
#
# What this means for the numbers:
#   • irradiate's trampoline eliminates per-mutant disk I/O — a structural
#     advantage, not just an optimization.
#   • Operator coverage differs; mutant counts will NOT match between tools.
#     This is expected and not a bug.
#   • ms/mutant is the fairest comparison metric (normalizes for count differences).
#   • We pin mutmut to 2.5.1 — the last stable release. mutmut 3.x crashes on
#     macOS (set_start_method #466, setproctitle #446).
#   • mutmut 2.x does NOT use the trampoline architecture mutmut 3.x introduced.
#     This comparison is disk-based vs trampoline, not old-mutmut vs new-mutmut.
#
# See bench/README.md for full methodology documentation.
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH_DIR="$ROOT/bench"

# ── Argument parsing ───────────────────────────────────────────────────────
TARGET="${1:-}"
if [ -z "$TARGET" ]; then
    echo "Usage: bash bench/compare.sh <target_name> [--runs N]" >&2
    echo "Available targets: simple_project, my_lib" >&2
    exit 1
fi
shift

RUNS="${BENCH_RUNS:-3}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs)
            RUNS="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# ── Load target config ─────────────────────────────────────────────────────
TARGET_FILE="$BENCH_DIR/targets/${TARGET}.sh"
if [ ! -f "$TARGET_FILE" ]; then
    echo "Error: no target config at $TARGET_FILE" >&2
    exit 1
fi

# shellcheck source=/dev/null
source "$TARGET_FILE"
# Expected exports from target file:
#   PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

echo "=== Benchmark: $TARGET ==="
echo "Project:       $PROJECT_DIR"
echo "Paths:         $PATHS_TO_MUTATE"
echo "Tests:         $TESTS_DIR"
echo "Python:        $PYTHON"
echo "Runs:          $RUNS (plus 1 warmup)"
echo

# ── Sanity checks ─────────────────────────────────────────────────────────
IRRADIATE_BIN="$ROOT/target/release/irradiate"

if [ ! -x "$IRRADIATE_BIN" ]; then
    echo "Error: $IRRADIATE_BIN not found. Run: bash bench/setup.sh" >&2
    exit 1
fi

# ── Result directory ──────────────────────────────────────────────────────
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULT_DIR="$BENCH_DIR/results/${TIMESTAMP}/${TARGET}"
mkdir -p "$RESULT_DIR"
echo "Results:       $RESULT_DIR"
echo

NCPU="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 2)"

# ── Helper: clean slate ───────────────────────────────────────────────────
clean_slate() {
    rm -rf "$PROJECT_DIR/mutants" "$PROJECT_DIR/.irradiate" "$PROJECT_DIR/.mutmut-cache"
}

# ── Helper: run one configuration ─────────────────────────────────────────
# run_config CONFIG_NAME RUN_NUMBER CMD...
run_config() {
    local config="$1"
    local run_n="$2"
    shift 2

    local out="$RESULT_DIR/${config}_run${run_n}_stdout.txt"
    local err="$RESULT_DIR/${config}_run${run_n}_stderr.txt"
    local time_file="$RESULT_DIR/${config}_run${run_n}_time.txt"

    echo "  run $run_n: $config"

    clean_slate

    # /usr/bin/time writes timing info to stderr; -l is macOS, -v is Linux
    local time_flag="-l"
    if /usr/bin/time -v true 2>/dev/null; then time_flag="-v"; fi
    {
        /usr/bin/time "$time_flag" "$@" \
            > >(tee "$out") \
            2> >(tee "$err" >&2)
    } 2>"$time_file" || true
    # Note: tool exit code may be non-zero (e.g., survived mutants); we don't fail on that.
    # The time output goes to time_file, tool stderr goes to err file.
}

# ── Helper: warmup run (discarded) ────────────────────────────────────────
warmup_run() {
    local config="$1"
    shift
    echo "  warmup: $config (discarded)"
    clean_slate
    "$@" > /dev/null 2>&1 || true
}

# ── Run irradiate pool (N workers) ────────────────────────────────────────
CONFIG="irradiate_pool_${NCPU}w"
echo "--- $CONFIG ---"
warmup_run "$CONFIG" \
    "$IRRADIATE_BIN" run \
        --paths-to-mutate "$PATHS_TO_MUTATE" \
        --tests-dir "$TESTS_DIR" \
        --workers "$NCPU" \
        --python "$PYTHON"

for i in $(seq 1 "$RUNS"); do
    (
        cd "$PROJECT_DIR"
        run_config "$CONFIG" "$i" \
            "$IRRADIATE_BIN" run \
                --paths-to-mutate "$PATHS_TO_MUTATE" \
                --tests-dir "$TESTS_DIR" \
                --workers "$NCPU" \
                --python "$PYTHON"
    )
done
echo

# ── Run irradiate pool (1 worker) ─────────────────────────────────────────
CONFIG="irradiate_pool_1w"
echo "--- $CONFIG ---"
warmup_run "$CONFIG" \
    "$IRRADIATE_BIN" run \
        --paths-to-mutate "$PATHS_TO_MUTATE" \
        --tests-dir "$TESTS_DIR" \
        --workers 1 \
        --python "$PYTHON"

for i in $(seq 1 "$RUNS"); do
    (
        cd "$PROJECT_DIR"
        run_config "$CONFIG" "$i" \
            "$IRRADIATE_BIN" run \
                --paths-to-mutate "$PATHS_TO_MUTATE" \
                --tests-dir "$TESTS_DIR" \
                --workers 1 \
                --python "$PYTHON"
    )
done
echo

# ── Run irradiate isolate ─────────────────────────────────────────────────
CONFIG="irradiate_isolate"
echo "--- $CONFIG ---"
warmup_run "$CONFIG" \
    "$IRRADIATE_BIN" run \
        --paths-to-mutate "$PATHS_TO_MUTATE" \
        --tests-dir "$TESTS_DIR" \
        --isolate \
        --python "$PYTHON"

for i in $(seq 1 "$RUNS"); do
    (
        cd "$PROJECT_DIR"
        run_config "$CONFIG" "$i" \
            "$IRRADIATE_BIN" run \
                --paths-to-mutate "$PATHS_TO_MUTATE" \
                --tests-dir "$TESTS_DIR" \
                --isolate \
                --python "$PYTHON"
    )
done
echo

# ── Run mutmut (N children) ───────────────────────────────────────────────
# mutmut 2.5.1 pinned — see header comment for apples-to-oranges context.
# mutmut 2.5.1 is sequential (no --max-children) and takes 60+ minutes on
# CI. Skip on CI by default; set BENCH_MUTMUT=1 to force.
MUTMUT_PYTHON="$BENCH_DIR/.venv/bin/python"
MUTMUT_PATH="$BENCH_DIR/.venv/bin:$PATH"
BENCH_MUTMUT="${BENCH_MUTMUT:-}"
if [ -n "${CI:-}" ] && [ "$BENCH_MUTMUT" != "1" ]; then
    echo "--- mutmut_1c --- (skipped on CI — set BENCH_MUTMUT=1 to enable)" >&2
elif [ ! -x "$MUTMUT_PYTHON" ]; then
    echo "Warning: $MUTMUT_PYTHON not found — skipping mutmut benchmarks." >&2
    echo "  Run: bash bench/setup.sh" >&2
else
    CONFIG="mutmut_1c"
    echo "--- $CONFIG ---"
    ( cd "$PROJECT_DIR" && PATH="$MUTMUT_PATH" warmup_run "$CONFIG" "$MUTMUT_PYTHON" -m mutmut run )

    for i in $(seq 1 "$RUNS"); do
        (
            cd "$PROJECT_DIR"
            PATH="$MUTMUT_PATH" run_config "$CONFIG" "$i" \
                "$MUTMUT_PYTHON" -m mutmut run
        )
    done
    echo
fi

# ── Generate summary ──────────────────────────────────────────────────────
echo "=== Generating summary ==="
uv run --python 3.12 "$BENCH_DIR/summarize.py" \
    "$RESULT_DIR" \
    --target "$TARGET" \
    --ncpu "$NCPU" \
    --runs "$RUNS"

echo
echo "Summary: $RESULT_DIR/summary.md"
echo "Raw data: $RESULT_DIR/raw_data.json"
echo
cat "$RESULT_DIR/summary.md"
