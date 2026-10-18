#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
.github/scripts/test-release-version-bump.sh
python3 scripts/test-e2e-version.py
python3 scripts/test-sdk-version.py
python3 scripts/test-native-portability.py
cargo test --locked --workspace
cargo test --locked --workspace --all-features
cargo fmt --manifest-path crates/tinyvoice-module/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path crates/tinyvoice-module/Cargo.toml --all-targets --all-features -- -D warnings
cargo test --locked --manifest-path crates/tinyvoice-module/Cargo.toml --all-features
case "${RUNNER_OS:-$(uname -s)}" in
  Windows|MINGW*|MSYS*|CYGWIN*) cargo test --locked --manifest-path crates/tinyvoice-hotkey-win/Cargo.toml ;;
esac
case "${RUNNER_OS:-$(uname -s)}" in
  Windows|MINGW*|MSYS*|CYGWIN*) ;;
  *) python3 scripts/test-package-layout.py
     python3 scripts/test-distro-cleanup.py ;;
esac
