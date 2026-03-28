#!/usr/bin/env bash
# bench/run_all.sh — Run benchmarks across all targets, produce aggregate report.
#
# Usage:
#   bash bench/run_all.sh [--runs N] [--targets "t1 t2 ..."]
#
# Examples:
#   bash bench/run_all.sh                           # all 6 targets, 3 runs
#   bash bench/run_all.sh --runs 1                  # quick smoke test
#   bash bench/run_all.sh --targets "markupsafe toolz"
#
# Environment overrides:
#   BENCH_RUNS=N           number of timed runs per config (default: 3)
#   BENCH_TARGETS="t1 t2"  space-separated target names
#   BENCH_MUTMUT=1         force mutmut runs (otherwise skipped on CI)
#   BENCH_ISOLATE=1        force irradiate isolate runs (otherwise skipped on CI)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DEFAULT_TARGETS="markupsafe itsdangerous toolz marshmallow more-itertools click"

RUNS="${BENCH_RUNS:-3}"
TARGETS="${BENCH_TARGETS:-$DEFAULT_TARGETS}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs)
            RUNS="$2"; shift 2 ;;
        --targets)
            TARGETS="$2"; shift 2 ;;
        *)
            echo "Unknown argument: $1" >&2
            echo "Usage: bash bench/run_all.sh [--runs N] [--targets \"t1 t2 ...\"]" >&2
            exit 1 ;;
    esac
done

export BENCH_RUNS="$RUNS"

# Shared timestamp so all targets land under one directory
export BENCH_TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
RESULTS_ROOT="$ROOT/bench/results/$BENCH_TIMESTAMP"

echo "=== irradiate vs mutmut benchmark ==="
echo "Targets:   $TARGETS"
echo "Runs:      $RUNS per config"
echo "Results:   $RESULTS_ROOT"
echo

# Ensure release binary exists
if [ ! -x "$ROOT/target/release/irradiate" ]; then
    echo "Release binary not found. Running setup..."
    bash "$ROOT/bench/setup.sh"
    echo
fi

# Run each target
FAILED=""
for target in $TARGETS; do
    echo ""
    echo "================================================================"
    echo "  TARGET: $target"
    echo "================================================================"
    if bash "$ROOT/bench/compare.sh" "$target"; then
        echo "  $target: done"
    else
        echo "  $target: FAILED (continuing with remaining targets)" >&2
        FAILED="$FAILED $target"
    fi
done

# Generate aggregate report
echo ""
echo "================================================================"
echo "  AGGREGATE REPORT"
echo "================================================================"
uv run --no-project --python 3.12 "$ROOT/bench/summarize.py" \
    "$RESULTS_ROOT" \
    --aggregate

echo
echo "Per-target summaries:"
for target in $TARGETS; do
    summary="$RESULTS_ROOT/$target/summary.md"
    if [ -f "$summary" ]; then
        echo "  $summary"
    fi
done
echo
echo "Aggregate report: $RESULTS_ROOT/aggregate.md"
echo "Aggregate data:   $RESULTS_ROOT/aggregate.json"

if [ -n "$FAILED" ]; then
    echo
    echo "WARNING: The following targets failed:$FAILED" >&2
fi

echo
cat "$RESULTS_ROOT/aggregate.md"
