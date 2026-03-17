#!/bin/bash
set -euo pipefail

# E2E test script for irradiate
# For now, just verify the build succeeds.
# Will be expanded in Spec 4 to test against mutmut e2e_projects.

echo "Building release binary..."
cargo build --release

echo "E2E tests: PASS (skeleton only)"
