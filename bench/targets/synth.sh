#!/usr/bin/env bash
# bench/targets/synth.sh — Target config for bench/targets/synth
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/bench/targets/synth"
export PATHS_TO_MUTATE="src/synth"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
