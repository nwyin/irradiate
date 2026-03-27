#!/usr/bin/env bash
# scripts/lima-bench.sh — Run the full benchmark suite inside a Lima Linux VM.
#
# Prerequisites:
#   brew install lima
#
# Usage:
#   bash scripts/lima-bench.sh                              # all targets, 3 runs
#   bash scripts/lima-bench.sh --runs 1 --targets markupsafe
#   bash scripts/lima-bench.sh --teardown                   # delete the VM
#
# The VM is persistent across runs so you only pay setup cost once.
# Results are written back to bench/results/ on the host (shared mount).
set -euo pipefail

VM_NAME="irradiate-bench"
VM_CPUS=4
VM_MEMORY="8GiB"
VM_DISK="30GiB"

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ── Argument parsing ──────────────────────────────────────────────────────
RUNS=""
TARGETS=""
TEARDOWN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs)     RUNS="$2"; shift 2 ;;
        --targets)  TARGETS="$2"; shift 2 ;;
        --teardown) TEARDOWN=true; shift ;;
        *)          echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

if $TEARDOWN; then
    echo "Deleting VM '$VM_NAME'..."
    limactl delete --force "$VM_NAME" 2>/dev/null || true
    echo "Done."
    exit 0
fi

# ── Ensure lima is installed ──────────────────────────────────────────────
if ! command -v limactl &>/dev/null; then
    echo "lima not found. Install with: brew install lima" >&2
    exit 1
fi

# ── Create VM if it doesn't exist ─────────────────────────────────────────
if ! limactl list --json | grep -q "\"name\":\"$VM_NAME\""; then
    echo "=== Creating Lima VM '$VM_NAME' (${VM_CPUS} CPUs, ${VM_MEMORY} RAM) ==="
    limactl create \
        --name "$VM_NAME" \
        --cpus "$VM_CPUS" \
        --memory "$VM_MEMORY" \
        --disk "$VM_DISK" \
        --mount-writable \
        --tty=false \
        template://default
    echo
fi

# ── Start VM if stopped ──────────────────────────────────────────────────
STATUS=$(limactl list --json | python3 -c "
import json, sys
for line in sys.stdin:
    vm = json.loads(line)
    if vm.get('name') == '$VM_NAME':
        print(vm.get('status', 'Unknown'))
        break
" 2>/dev/null || echo "Unknown")

if [ "$STATUS" != "Running" ]; then
    echo "=== Starting VM '$VM_NAME' ==="
    limactl start "$VM_NAME"
    echo
fi

# ── Helper: run command inside VM ─────────────────────────────────────────
vm() {
    limactl shell "$VM_NAME" "$@"
}

# ── One-time provisioning ────────────────────────────────────────────────
# Check if Rust is already installed to skip re-provisioning
if ! vm bash -c "command -v cargo" &>/dev/null; then
    echo "=== Provisioning VM (Rust, uv, Python 3.12) ==="

    # System packages
    vm sudo apt-get update -qq
    vm sudo apt-get install -y -qq build-essential pkg-config libssl-dev python3.12 python3.12-venv git

    # Rust
    vm bash -c "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y"

    # uv
    vm bash -c "curl -LsSf https://astral.sh/uv/install.sh | sh"

    echo
fi

# ── Run benchmarks ───────────────────────────────────────────────────────
echo "=== Running benchmarks ==="
echo "Repo:    $REPO_DIR"
echo "Targets: ${TARGETS:-all}"
echo "Runs:    ${RUNS:-3}"
echo

ENV_VARS="export PATH=\"\$HOME/.cargo/bin:\$HOME/.local/bin:\$PATH\""
BENCH_CMD="cd '$REPO_DIR' && bash bench/setup.sh && "

BENCH_ENV=""
[ -n "$RUNS" ]    && BENCH_ENV="${BENCH_ENV}BENCH_RUNS=$RUNS "
[ -n "$TARGETS" ] && BENCH_ENV="${BENCH_ENV}BENCH_TARGETS='$TARGETS' "
BENCH_ENV="${BENCH_ENV}BENCH_MUTMUT=1 "

BENCH_CMD="${BENCH_CMD}${BENCH_ENV}bash bench/run_all.sh"

vm bash -c "$ENV_VARS && $BENCH_CMD"

# ── Report ───────────────────────────────────────────────────────────────
LATEST=$(ls -td "$REPO_DIR/bench/results"/*/ 2>/dev/null | head -1)
if [ -n "$LATEST" ] && [ -f "$LATEST/aggregate.md" ]; then
    echo
    echo "=== Results ==="
    cat "$LATEST/aggregate.md"
    echo
    echo "Full results: $LATEST"
else
    echo
    echo "Results directory: $REPO_DIR/bench/results/"
fi
