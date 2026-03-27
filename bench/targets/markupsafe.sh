#!/usr/bin/env bash
# bench/targets/markupsafe.sh — Target config for markupsafe (src-layout)
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/corpora/markupsafe"
export PATHS_TO_MUTATE="src/markupsafe"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
