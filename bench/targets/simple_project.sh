#!/usr/bin/env bash
# bench/targets/simple_project.sh — Target config for tests/fixtures/simple_project
# Source this file; it exports: PROJECT_DIR, PATHS_TO_MUTATE, TESTS_DIR, PYTHON

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

export PROJECT_DIR="$ROOT/tests/fixtures/simple_project"
export PATHS_TO_MUTATE="src"
export TESTS_DIR="tests"
export PYTHON="$PROJECT_DIR/.venv/bin/python3"
