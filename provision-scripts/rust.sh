#!/bin/bash
# Installs Rust with rustup. rustup adds ~/.cargo/bin to the PATH in
# ~/.profile, so later scripts (login shells) find cargo right away.
set -euo pipefail

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal -c clippy -c rustfmt
