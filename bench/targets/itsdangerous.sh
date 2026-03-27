#!/usr/bin/env bash
# bench/targets/itsdangerous.sh — Target config for itsdangerous (src-layout)
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/corpora/itsdangerous"
export PATHS_TO_MUTATE="src/itsdangerous"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
