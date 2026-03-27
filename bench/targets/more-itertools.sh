#!/usr/bin/env bash
# bench/targets/more-itertools.sh — Target config for more-itertools (flat layout)
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/corpora/more-itertools"
export PATHS_TO_MUTATE="more_itertools"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
