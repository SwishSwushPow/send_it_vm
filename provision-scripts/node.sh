#!/bin/bash
# Installs the current Node.js LTS into /usr/local. Debian ships Node.js 20,
# older than Pi and REA need.

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
