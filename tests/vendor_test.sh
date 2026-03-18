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

# Track results per repo
declare -A REPO_STATUS
declare -A REPO_DETAIL

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
    timeout 600 "$BINARY" run \
        --paths-to-mutate "$paths_to_mutate" \
        --tests-dir "$tests_dir" \
        --python "$repo_dir/.venv/bin/python3" \
        --workers 2 \
        --timeout-multiplier 5 \
        --no-stats \
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
# Repo: click
# ---------------------------------------------------------------------------

REPO="click"
log "=== $REPO ==="
{
    clone_repo "$REPO" "https://github.com/pallets/click.git"
    setup_venv "$REPO" 'uv pip install -e ".[testing]" 2>&1'

    if run_irradiate "$REPO" "src/click" "tests"; then
        REPO_STATUS["$REPO"]="PASS"
    else
        local_exit=$?
        if [ "$local_exit" -eq 124 ]; then
            REPO_STATUS["$REPO"]="FAIL"
            REPO_DETAIL["$REPO"]="timeout after 600s — see log"
        else
            # Non-zero exit from irradiate is not necessarily a crash —
            # check for panics in log to distinguish crash from "no tests" etc.
            log_file="$RESULTS_DIR/${REPO}.log"
            if grep -q -i "panic\|thread '.*' panicked" "$log_file" 2>/dev/null; then
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="PANIC — see log"
            else
                # Non-zero but no panic: treat as informational failure
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="irradiate exited $local_exit — see log"
            fi
        fi
    fi
} 2>&1 | tee -a "$RESULTS_DIR/${REPO}.log" || true

# If PASS, enrich with parsed detail
if [ "${REPO_STATUS[$REPO]:-}" = "PASS" ]; then
    REPO_DETAIL["$REPO"]="$(parse_summary "$REPO")"
fi

# ---------------------------------------------------------------------------
# Repo: more-itertools
# ---------------------------------------------------------------------------

REPO="more-itertools"
log "=== $REPO ==="
{
    clone_repo "$REPO" "https://github.com/more-itertools/more-itertools.git"
    setup_venv "$REPO" 'uv pip install -e "." pytest 2>&1'

    if run_irradiate "$REPO" "more_itertools" "tests"; then
        REPO_STATUS["$REPO"]="PASS"
    else
        local_exit=$?
        if [ "$local_exit" -eq 124 ]; then
            REPO_STATUS["$REPO"]="FAIL"
            REPO_DETAIL["$REPO"]="timeout after 600s — see log"
        else
            log_file="$RESULTS_DIR/${REPO}.log"
            if grep -q -i "panic\|thread '.*' panicked" "$log_file" 2>/dev/null; then
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="PANIC — see log"
            else
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="irradiate exited $local_exit — see log"
            fi
        fi
    fi
} 2>&1 | tee -a "$RESULTS_DIR/${REPO}.log" || true

if [ "${REPO_STATUS[$REPO]:-}" = "PASS" ]; then
    REPO_DETAIL["$REPO"]="$(parse_summary "$REPO")"
fi

# ---------------------------------------------------------------------------
# Repo: httpx
# ---------------------------------------------------------------------------

REPO="httpx"
log "=== $REPO ==="
{
    clone_repo "$REPO" "https://github.com/encode/httpx.git"
    setup_venv "$REPO" 'uv pip install -e "." pytest respx trustme anyio trio 2>&1'

    if run_irradiate "$REPO" "httpx" "tests"; then
        REPO_STATUS["$REPO"]="PASS"
    else
        local_exit=$?
        if [ "$local_exit" -eq 124 ]; then
            REPO_STATUS["$REPO"]="FAIL"
            REPO_DETAIL["$REPO"]="timeout after 600s — see log"
        else
            log_file="$RESULTS_DIR/${REPO}.log"
            if grep -q -i "panic\|thread '.*' panicked" "$log_file" 2>/dev/null; then
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="PANIC — see log"
            else
                REPO_STATUS["$REPO"]="FAIL"
                REPO_DETAIL["$REPO"]="irradiate exited $local_exit — see log"
            fi
        fi
    fi
} 2>&1 | tee -a "$RESULTS_DIR/${REPO}.log" || true

if [ "${REPO_STATUS[$REPO]:-}" = "PASS" ]; then
    REPO_DETAIL["$REPO"]="$(parse_summary "$REPO")"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

echo ""
echo "=== Vendor Test Summary ==="
for repo in click more-itertools httpx; do
    status="${REPO_STATUS[$repo]:-SKIP}"
    detail="${REPO_DETAIL[$repo]:-}"
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
