#!/usr/bin/env bash
# Count source lines of code, excluding tests and blanks.
# Splits inline #[cfg(test)] blocks from src, counts harness Python separately.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

verbose=false
[[ "${1:-}" == "-v" ]] && verbose=true

# --- Rust src (excluding test blocks) ---

# Test code: #[cfg(test)] to EOF, or entire test-only files.

rust_src_total=0
rust_test_total=0

declare -a module_lines=()

test_only_files=(src/mutation_tests.rs)

for f in src/*.rs; do
    is_test_only=false
    for tf in "${test_only_files[@]}"; do
        [[ "$f" == "$tf" ]] && is_test_only=true && break
    done

    if $is_test_only; then
        lines=$(grep -cv '^[[:space:]]*$' "$f" 2>/dev/null || echo 0)
        rust_test_total=$((rust_test_total + lines))
        continue
    fi

    cfg_test_line=$(grep -n '#\[cfg(test)\]' "$f" | head -1 | cut -d: -f1 || true)

    if [[ -n "$cfg_test_line" ]]; then
        src_lines=$(head -n "$((cfg_test_line - 1))" "$f" | grep -cv '^[[:space:]]*$' 2>/dev/null || echo 0)
        test_lines=$(tail -n +"$cfg_test_line" "$f" | grep -cv '^[[:space:]]*$' 2>/dev/null || echo 0)
    else
        src_lines=$(grep -cv '^[[:space:]]*$' "$f" 2>/dev/null || echo 0)
        test_lines=0
    fi

    module_lines+=("$(printf '%5d  %s' "$src_lines" "${f#src/}")")
    rust_src_total=$((rust_src_total + src_lines))
    rust_test_total=$((rust_test_total + test_lines))
done

# --- Rust test files in tests/ ---

for f in tests/*.rs; do
    [[ -f "$f" ]] || continue
    lines=$(grep -cv '^[[:space:]]*$' "$f" 2>/dev/null || echo 0)
    rust_test_total=$((rust_test_total + lines))
done

# --- e2e/shell tests ---

for f in tests/e2e.sh tests/vendor_test.sh; do
    [[ -f "$f" ]] || continue
    lines=$(grep -cv '^[[:space:]]*$' "$f" 2>/dev/null || echo 0)
    rust_test_total=$((rust_test_total + lines))
done

# --- Python harness ---

py_src_total=0
declare -a py_module_lines=()

for f in harness/*.py; do
    [[ -f "$f" ]] || continue
    lines=$(grep -cv '^[[:space:]]*$' "$f" 2>/dev/null || echo 0)
    py_module_lines+=("$(printf '%5d  %s' "$lines" "${f#harness/}")")
    py_src_total=$((py_src_total + lines))
done

# --- output ---

total_src=$((rust_src_total + py_src_total))
total=$((total_src + rust_test_total))

printf "\n"

if $verbose; then
    printf "  Rust src by module:\n"
    # sort descending by line count
    printf '%s\n' "${module_lines[@]}" | sort -rn | while read -r line; do
        printf "    %s\n" "$line"
    done
    printf "  %5d  total\n\n" "$rust_src_total"

    printf "  Python harness by module:\n"
    printf '%s\n' "${py_module_lines[@]}" | sort -rn | while read -r line; do
        printf "    %s\n" "$line"
    done
    printf "  %5d  total\n\n" "$py_src_total"
fi

printf "  %-24s %6d lines\n" "Rust src" "$rust_src_total"
printf "  %-24s %6d lines\n" "Python harness" "$py_src_total"
printf "  %-24s %6d lines\n" "─── src total" "$total_src"
printf "\n"
printf "  %-24s %6d lines\n" "Tests (unit+e2e)" "$rust_test_total"
printf "\n"
printf "  %-24s %6d lines\n" "Total" "$total"
printf "\n"
