#!/usr/bin/env bash
# scripts/run_vendor_tests.sh — Run irradiate on vendor corpora; produce vendor_results.json.
#
# Usage:
#   bash scripts/run_vendor_tests.sh [-o output.json]
#
# For each corpus in bench/corpora/ (markupsafe, click, httpx):
#   - status "not available"  → corpus dir or .venv/bin/python missing, or pytest not installed
#   - status "fail"           → irradiate ran but produced no parseable results
#   - status "pass"           → results obtained
#
# Output format (matches generate_report.py --vendor-results):
#   {"run_date": "YYYY-MM-DD", "repos": [...]}
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
CORPORA_DIR="$ROOT_DIR/bench/corpora"
IRRADIATE_BIN="$ROOT_DIR/target/release/irradiate"
OUTPUT_FILE="vendor_results.json"

while [[ $# -gt 0 ]]; do
    case "$1" in
        -o|--output) OUTPUT_FILE="$2"; shift 2 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# Ensure output path is absolute
if [[ "$OUTPUT_FILE" != /* ]]; then
    OUTPUT_FILE="$(pwd)/$OUTPUT_FILE"
fi

RUN_DATE="$(date +%Y-%m-%d)"
CORPORA=(markupsafe click httpx)

echo "=== run_vendor_tests.sh ===" >&2
echo "  Corpora dir:  $CORPORA_DIR" >&2
echo "  Irradiate:    $IRRADIATE_BIN" >&2
echo "  Output:       $OUTPUT_FILE" >&2
echo "" >&2

if [ ! -x "$IRRADIATE_BIN" ]; then
    echo "ERROR: irradiate binary not found at $IRRADIATE_BIN" >&2
    echo "Run: cargo build --release" >&2
    exit 1
fi

# Temp dir for intermediate files (auto-cleaned on exit)
WORK_DIR=$(mktemp -d)
trap 'rm -rf "$WORK_DIR"' EXIT

ENTRIES_FILE="$WORK_DIR/entries.jsonl"

# ---------------------------------------------------------------------------
# get_corpus_paths <corpus_dir>
# Outputs lines like "paths=src/markupsafe" and "tests=tests" by reading
# pyproject.toml [tool.irradiate] or [tool.mutmut], or falls back to
# heuristic directory detection.
# ---------------------------------------------------------------------------
get_corpus_paths() {
    local corpus_dir="$1"
    local name
    name="$(basename "$corpus_dir")"

    # Try pyproject.toml first (tomllib is stdlib in Python 3.11+)
    if [ -f "$corpus_dir/pyproject.toml" ]; then
        PYPROJECT_PATH="$corpus_dir/pyproject.toml" python3 - <<'PYEOF' 2>/dev/null || true
import os, sys
pyproject = os.environ.get('PYPROJECT_PATH', '')
if not pyproject:
    sys.exit(0)
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib  # type: ignore
    except ImportError:
        sys.exit(0)
try:
    with open(pyproject, 'rb') as f:
        d = tomllib.load(f)
    irr = d.get('tool', {}).get('irradiate', {})
    mm = d.get('tool', {}).get('mutmut', {})
    paths = (irr.get('paths_to_mutate') or mm.get('paths_to_mutate') or '').strip()
    tests = (irr.get('tests_dir') or mm.get('tests_dir') or '').strip()
    if paths:
        print('paths=' + paths)
    if tests:
        print('tests=' + tests)
except Exception:
    pass
PYEOF
    fi

    # Heuristic fallback: detect source layout
    local paths_out=""
    if [ -d "$corpus_dir/src/$name" ]; then
        paths_out="src/$name"
    elif [ -d "$corpus_dir/$name" ]; then
        paths_out="$name"
    else
        paths_out="src"
    fi
    echo "paths_fallback=$paths_out"
}

# ---------------------------------------------------------------------------
# parse_irradiate_stats <stderr_file>
# Prints three lines: total, killed, survived (empty string if not found)
# ---------------------------------------------------------------------------
parse_irradiate_stats() {
    local stderr_file="$1"
    python3 - <<PYEOF 2>/dev/null
import re, pathlib
text = pathlib.Path("$stderr_file").read_text(errors="replace") if pathlib.Path("$stderr_file").exists() else ""
m_total   = re.search(r'Mutation testing complete \((\d+) mutants', text)
m_killed  = re.search(r'Killed:\s+(\d+)', text)
m_survived = re.search(r'Survived:\s+(\d+)', text)
print(m_total.group(1)    if m_total    else '')
print(m_killed.group(1)   if m_killed   else '')
print(m_survived.group(1) if m_survived else '')
PYEOF
}

# ---------------------------------------------------------------------------
# Main loop
# ---------------------------------------------------------------------------
for name in "${CORPORA[@]}"; do
    corpus_dir="$CORPORA_DIR/$name"
    echo "--- $name ---" >&2

    # --- availability checks ---
    if [ ! -d "$corpus_dir" ]; then
        echo "  status: not available (directory missing: $corpus_dir)" >&2
        echo "{\"name\": \"$name\", \"status\": \"not available\"}" >> "$ENTRIES_FILE"
        continue
    fi

    venv_python="$corpus_dir/.venv/bin/python"
    if [ ! -x "$venv_python" ]; then
        echo "  status: not available (.venv/bin/python missing)" >&2
        echo "{\"name\": \"$name\", \"status\": \"not available\"}" >> "$ENTRIES_FILE"
        continue
    fi

    if ! "$venv_python" -c "import pytest" 2>/dev/null; then
        echo "  status: not available (pytest not installed in .venv)" >&2
        echo "{\"name\": \"$name\", \"status\": \"not available\"}" >> "$ENTRIES_FILE"
        continue
    fi

    # --- determine paths_to_mutate and tests_dir ---
    config_out="$(get_corpus_paths "$corpus_dir")"

    paths_to_mutate=""
    tests_dir=""

    if echo "$config_out" | grep -q '^paths='; then
        paths_to_mutate="$(echo "$config_out" | grep '^paths=' | head -1 | sed 's/^paths=//')"
    fi
    if echo "$config_out" | grep -q '^paths_fallback='; then
        fallback="$(echo "$config_out" | grep '^paths_fallback=' | head -1 | sed 's/^paths_fallback=//')"
        if [ -z "$paths_to_mutate" ]; then
            paths_to_mutate="$fallback"
        fi
    fi
    if echo "$config_out" | grep -q '^tests='; then
        tests_dir="$(echo "$config_out" | grep '^tests=' | head -1 | sed 's/^tests=//')"
    fi
    if [ -z "$tests_dir" ]; then
        tests_dir="tests"
    fi
    if [ -z "$paths_to_mutate" ]; then
        paths_to_mutate="src"
    fi

    echo "  paths_to_mutate: $paths_to_mutate" >&2
    echo "  tests_dir:       $tests_dir" >&2
    echo "  python:          $venv_python" >&2

    # --- clean up prior irradiate state ---
    rm -rf "$corpus_dir/mutants" "$corpus_dir/.irradiate"

    # --- run irradiate ---
    stderr_file="$WORK_DIR/${name}_stderr.txt"
    irr_exit=0
    (cd "$corpus_dir" && \
        "$IRRADIATE_BIN" run \
            --paths-to-mutate "$paths_to_mutate" \
            --tests-dir "$tests_dir" \
            --python "$venv_python" \
        > /dev/null 2>"$stderr_file") || irr_exit=$?

    echo "  irradiate exit code: $irr_exit" >&2

    # --- parse stats ---
    stats_lines="$(parse_irradiate_stats "$stderr_file")"
    total="$(echo   "$stats_lines" | sed -n '1p')"
    killed="$(echo  "$stats_lines" | sed -n '2p')"
    survived="$(echo "$stats_lines" | sed -n '3p')"

    if [ -z "$total" ] || [ -z "$killed" ] || [ -z "$survived" ]; then
        echo "  status: fail (could not parse stats from irradiate output)" >&2
        echo "  --- last 20 lines of irradiate stderr ---" >&2
        tail -20 "$stderr_file" | sed 's/^/    /' >&2
        echo "{\"name\": \"$name\", \"status\": \"fail\"}" >> "$ENTRIES_FILE"
        continue
    fi

    # Calculate mutation score
    score=$(python3 -c "
k, s = $killed, $survived
t = k + s
print(f'{k/t*100:.1f}' if t > 0 else '0.0')
" 2>/dev/null || echo "0.0")

    echo "  status: pass  mutants=$total  killed=$killed  survived=$survived  score=${score}%" >&2
    echo "{\"name\": \"$name\", \"status\": \"pass\", \"mutants\": $total, \"killed\": $killed, \"survived\": $survived, \"mutation_score_pct\": $score}" >> "$ENTRIES_FILE"
done

# ---------------------------------------------------------------------------
# Assemble final JSON using Python
# ---------------------------------------------------------------------------
ENTRIES_FILE_ARG="$ENTRIES_FILE" OUTPUT_FILE_ARG="$OUTPUT_FILE" RUN_DATE_ARG="$RUN_DATE" \
python3 - <<'PYEOF'
import json, os, pathlib

entries_path = pathlib.Path(os.environ['ENTRIES_FILE_ARG'])
output_path  = pathlib.Path(os.environ['OUTPUT_FILE_ARG'])
run_date     = os.environ['RUN_DATE_ARG']

entries = []
if entries_path.exists():
    for line in entries_path.read_text().splitlines():
        line = line.strip()
        if line:
            entries.append(json.loads(line))

result = {
    "run_date": run_date,
    "repos": entries,
}

output_path.parent.mkdir(parents=True, exist_ok=True)
output_path.write_text(json.dumps(result, indent=2) + "\n")
print(f"Written: {output_path}  ({len(entries)} repos)")
PYEOF

echo "" >&2
echo "=== Done ===" >&2
