#!/usr/bin/env bash
set -euo pipefail

# Enforce panic-free runtime code paths across both executable and library
# targets. We intentionally do not lint tests here because panic-oriented
# assertions in tests are acceptable.
cargo clippy --bins --lib -- -D clippy::unwrap_used -D clippy::expect_used
