#!/bin/sh
# Local-only verification; run from the AI48Proxy repository root.
set -eu

if [ ! -f Cargo.toml ] || [ ! -f Cargo.lock ]; then
    echo 'Run from the AI48Proxy repository root.' >&2
    exit 2
fi
for script in scripts/*.sh; do
    sh -n "$script"
done
rustc --version
cargo --version
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked --release
printf '%s\n' 'Local verification passed. No live Home or provider acceptance was performed.'
