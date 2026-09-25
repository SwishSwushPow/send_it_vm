#!/bin/bash
# Installs the Pi coding agent (pi.dev). Pi needs Node.js 22.19 or newer,
# which Debian doesn't ship, so install the current Node.js LTS into
# /usr/local first; pi's installer only offers to install Node.js itself when
# it can ask on a terminal. Without a terminal it installs without asking,
# into ~/.local/bin, which Debian's ~/.profile puts on the PATH.
set -euo pipefail

command -v xz > /dev/null || sudo apt-get -y install --no-install-recommends xz-utils

dist=https://nodejs.org/dist/latest-v24.x
tmp=$(mktemp -d)
curl -fsSL "$dist/SHASUMS256.txt" -o "$tmp/SHASUMS256.txt"
file=$(awk '$2 ~ /-linux-arm64\.tar\.xz$/ { print $2 }' "$tmp/SHASUMS256.txt")
curl -fsSL "$dist/$file" -o "$tmp/$file"
(cd "$tmp" && grep " $file\$" SHASUMS256.txt | sha256sum -c -)
sudo tar -xJf "$tmp/$file" -C /usr/local --strip-components=1 --no-same-owner \
    --exclude=CHANGELOG.md --exclude=LICENSE --exclude=README.md
rm -rf "$tmp"

mkdir -p ~/.local/bin
export PATH="$HOME/.local/bin:$PATH"
curl -fsSL https://pi.dev/install.sh | sh
