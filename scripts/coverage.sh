#!/usr/bin/env bash
# Generate a code coverage report (HTML + LCOV) for the whole workspace.
# Report-only: coverage is never gated in CI.
set -euo pipefail
cd "$(dirname "$0")/.."

# Capture coverage from child processes spawned by subprocess tests too.
export LLVM_PROFILE_FILE="$(pwd)/target/profraw/rgx-%p-%m.profraw"
rm -rf target/profraw

cargo llvm-cov --workspace --all-targets \
    --lcov --output-path target/coverage.lcov
cargo llvm-cov --workspace --all-targets \
    --html --output-dir target/coverage

echo
echo "Coverage report:    target/coverage/html/index.html"
echo "LCOV data:          target/coverage.lcov"