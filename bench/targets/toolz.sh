#!/usr/bin/env bash
# bench/targets/toolz.sh — Target config for toolz (flat layout, tests inside package)
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/corpora/toolz"
export PATHS_TO_MUTATE="toolz"
export TESTS_DIR="toolz/tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
