#!/usr/bin/env bash
# bench/targets/tinygrad.sh — Target config for tinygrad (flat layout)
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/corpora/tinygrad"
export PATHS_TO_MUTATE="tinygrad"
export TESTS_DIR="test"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
