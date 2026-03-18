#!/usr/bin/env bash
# Smoke test irradiate against real-world Python projects.
# Failures are expected and informative — they tell us what to fix.
# NOT intended for CI — too slow. Run manually.
#
# Usage: bash tests/vendor_test.sh
#
# Exit code: always 0. Check the summary for per-repo pass/fail.
set -uo pipefail  # NOTE: no -e, we want to continue after failures

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"
BINARY="$ROOT_DIR/target/debug/irradiate"
VENDOR_DIR="$SCRIPT_DIR/vendor_repos"
RESULTS_DIR="$VENDOR_DIR/_results"

# Track results per repo (parallel arrays — bash 3 compatible)
REPOS=(click more-itertools httpx)
STATUS_click="SKIP"
STATUS_more_itertools="SKIP"
STATUS_httpx="SKIP"
DETAIL_click=""
DETAIL_more_itertools=""
DETAIL_httpx=""

# Helpers to set/get status using indirect variable references (bash 3 safe)
set_status() { local key="STATUS_${1//-/_}"; eval "$key=\$2"; }
get_status() { local key="STATUS_${1//-/_}"; echo "${!key}"; }
set_detail() { local key="DETAIL_${1//-/_}"; eval "$key=\$2"; }
get_detail() { local key="DETAIL_${1//-/_}"; echo "${!key}"; }

mkdir -p "$RESULTS_DIR"

# ---------------------------------------------------------------------------
# Helper functions
# ---------------------------------------------------------------------------

log() {
    echo "[$(date '+%H:%M:%S')] $*"
}

# clone_repo <name> <url>
# Idempotent: skips if already exists.
clone_repo() {
    local name="$1"
    local url="$2"
    local dest="$VENDOR_DIR/$name"
    if [ -d "$dest/.git" ]; then
        log "$name: repo already cloned, skipping"
    else
        log "$name: cloning $url ..."
        git clone --depth=1 "$url" "$dest" 2>&1
        log "$name: clone done"
    fi
}

# setup_venv <name> <install_cmd>
# Creates .venv inside the repo dir and installs deps.
setup_venv() {
    local name="$1"
    local install_cmd="$2"
    local repo_dir="$VENDOR_DIR/$name"
    if [ ! -d "$repo_dir/.venv" ]; then
        log "$name: creating venv..."
        (cd "$repo_dir" && uv venv --python 3.12 2>&1)
        log "$name: installing deps: $install_cmd"
        (cd "$repo_dir" && eval "$install_cmd" 2>&1)
        log "$name: deps installed"
    else
        log "$name: venv already exists, skipping setup"
    fi
}

# run_with_timeout <seconds> <command...>
# Portable timeout: uses gtimeout (Homebrew) or timeout (Linux), falls back to no timeout.
run_with_timeout() {
    local secs="$1"
    shift
    if command -v gtimeout &>/dev/null; then
        gtimeout "$secs" "$@"
    elif command -v timeout &>/dev/null; then
        timeout "$secs" "$@"
    else
        # No timeout available — run directly
        "$@"
    fi
}

# run_irradiate <name> <paths_to_mutate> <tests_dir>
# Runs irradiate with a 10-minute timeout.
# Returns 0 if irradiate ran without crashing/hanging, 1 otherwise.
run_irradiate() {
    local name="$1"
    local paths_to_mutate="$2"
    local tests_dir="$3"
    local repo_dir="$VENDOR_DIR/$name"
    local log_file="$RESULTS_DIR/${name}.log"

    log "$name: cleaning previous run artifacts..."
    rm -rf "$repo_dir/mutants" "$repo_dir/.irradiate"

    log "$name: running irradiate (timeout 600s)..."
    log "$name: log -> $log_file"

    local exit_code=0
    (cd "$repo_dir" && run_with_timeout 600 "$BINARY" run \
        --paths-to-mutate "$paths_to_mutate" \
        --tests-dir "$tests_dir" \
        --python "$repo_dir/.venv/bin/python3" \
        --workers 2 \
        --timeout-multiplier 5 \
        --no-stats) \
        2>&1 | tee "$log_file" || exit_code=$?

    return $exit_code
}

# parse_summary <name>
# Reads the log and extracts mutant counts for the summary line.
parse_summary() {
    local name="$1"
    local log_file="$RESULTS_DIR/${name}.log"

    if [ ! -f "$log_file" ]; then
        echo "(no log file)"
        return
    fi

    # Look for panic / ICE
    if grep -q -i "panic\|thread '.*' panicked\|SIGSEGV\|Segmentation fault" "$log_file"; then
        echo "PANIC detected — see log"
        return
    fi

    # Look for parse errors from irradiate
    if grep -q -i "parse error\|failed to parse\|SyntaxError" "$log_file"; then
        echo "parse errors detected — see log"
        return
    fi

    # Try to extract mutant counts from irradiate output lines like:
    #   "142 mutants | 89 killed | 41 survived | 12 no-tests"
    # or similar summary lines.
    local summary
    summary=$(grep -o '[0-9]* mutant[s]*' "$log_file" | tail -1 || true)
    local killed
    killed=$(grep -o '[0-9]* killed' "$log_file" | tail -1 || true)
    local survived
    survived=$(grep -o '[0-9]* survived' "$log_file" | tail -1 || true)
    local no_tests
    no_tests=$(grep -o '[0-9]* no.test[s]*' "$log_file" | tail -1 || true)

    if [ -n "$summary" ] || [ -n "$killed" ] || [ -n "$survived" ]; then
        local parts=()
        [ -n "$summary" ]   && parts+=("$summary")
        [ -n "$killed" ]    && parts+=("$killed")
        [ -n "$survived" ]  && parts+=("$survived")
        [ -n "$no_tests" ]  && parts+=("$no_tests")
        local IFS=', '
        echo "${parts[*]}"
    else
        # Check if we got zero mutants (may mean all fns were skipped)
        if grep -q -i "0 mutants\|no mutants" "$log_file"; then
            echo "0 mutants produced (all functions may have been skipped)"
        else
            echo "ran (no structured summary found)"
        fi
    fi
}

# ---------------------------------------------------------------------------
# Build irradiate first
# ---------------------------------------------------------------------------

log "Building irradiate..."
if ! cargo build --manifest-path="$ROOT_DIR/Cargo.toml" 2>&1; then
    echo "ERROR: cargo build failed — cannot run vendor tests"
    exit 0
fi
log "Build OK"

# ---------------------------------------------------------------------------
# Run each repo
# ---------------------------------------------------------------------------

test_repo() {
    local repo="$1"
    local url="$2"
    local install_cmd="$3"
    local paths_to_mutate="$4"
    local tests_dir="$5"

    log "=== $repo ==="

    clone_repo "$repo" "$url"
    setup_venv "$repo" "$install_cmd"

    local exit_code=0
    run_irradiate "$repo" "$paths_to_mutate" "$tests_dir" || exit_code=$?

    if [ "$exit_code" -eq 0 ]; then
        set_status "$repo" "PASS"
        set_detail "$repo" "$(parse_summary "$repo")"
    elif [ "$exit_code" -eq 124 ]; then
        set_status "$repo" "FAIL"
        set_detail "$repo" "timeout after 600s — see log"
    else
        local log_file="$RESULTS_DIR/${repo}.log"
        if grep -q -i "panic\|thread '.*' panicked" "$log_file" 2>/dev/null; then
            set_status "$repo" "FAIL"
            set_detail "$repo" "PANIC — see log"
        else
            set_status "$repo" "FAIL"
            set_detail "$repo" "irradiate exited $exit_code — see log"
        fi
    fi
}

test_repo "click" \
    "https://github.com/pallets/click.git" \
    'uv pip install -e ".[testing]" 2>&1' \
    "src/click" "tests"

test_repo "more-itertools" \
    "https://github.com/more-itertools/more-itertools.git" \
    'uv pip install -e "." pytest 2>&1' \
    "more_itertools" "tests"

test_repo "httpx" \
    "https://github.com/encode/httpx.git" \
    'uv pip install -e "." pytest respx trustme anyio trio 2>&1' \
    "httpx" "tests"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "=== Vendor Test Summary ==="
for repo in "${REPOS[@]}"; do
    status="$(get_status "$repo")"
    detail="$(get_detail "$repo")"
    if [ -n "$detail" ]; then
        printf "%-20s %s (%s)\n" "${repo}:" "$status" "$detail"
    else
        printf "%-20s %s\n" "${repo}:" "$status"
    fi
done
echo ""
echo "Logs: $RESULTS_DIR/"
echo ""

# Always exit 0 — failures are informational
exit 0
