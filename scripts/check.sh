#!/usr/bin/env bash
# Run the same gates CI runs, locally, in order, stopping at the first failure.
#   ./scripts/check.sh            # fmt + clippy + tests + speed budgets + release build
#   ./scripts/check.sh quick      # skip the speed budgets and the release build
set -euo pipefail
cd "$(dirname "$0")/.."

step() {
    printf '\n\033[1m== %s ==\033[0m\n' "$1"
}

step "cargo fmt --check"
cargo fmt --check

step "cargo clippy --all-targets -- -D warnings"
cargo clippy --all-targets -- -D warnings

step "cargo test --all-targets"
cargo test --all-targets

if [[ "${1:-}" != "quick" ]]; then
    step "cargo test --release benchmarks (speed budgets)"
    cargo test --release benchmarks -- --nocapture

    step "cargo build --release"
    cargo build --release
fi

printf '\n\033[1;32mall checks passed\033[0m\n'
