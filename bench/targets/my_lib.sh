#!/usr/bin/env bash
# bench/targets/my_lib.sh — Target config for vendor/mutmut/e2e_projects/my_lib
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON
#
# Note: my_lib already has [tool.mutmut] in its pyproject.toml (debug = true).
# mutmut reads this config automatically when run from PROJECT_DIR.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/vendor/mutmut/e2e_projects/my_lib"
export PATHS_TO_MUTATE="src"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
