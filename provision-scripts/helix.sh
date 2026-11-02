#!/bin/bash
# Installs the Helix editor from its official release build, as
# /usr/local/bin/hx, and puts its runtime files, the tree-sitter grammars
# already compiled, and config in ~/.config/helix.

version=25.07.1
sha256=ce23fa8d395e633e3e54c052012f11965d91d8d5c2bfa659685f50430b4f8175
name=helix-$version-aarch64-linux

tmp=$(mktemp -d)
curl -fsSL -o "$tmp/helix.tar.xz" \
  "https://github.com/helix-editor/helix/releases/download/$version/$name.tar.xz"
echo "$sha256  $tmp/helix.tar.xz" | sha256sum -c -
tar -xJf "$tmp/helix.tar.xz" -C "$tmp"

sudo install -m 755 "$tmp/$name/hx" /usr/local/bin/hx
mkdir -p ~/.config/helix
cp -r "$tmp/$name/runtime" ~/.config/helix/runtime

# set default theme
echo 'theme = "onedark"' >> ~/.config/helix/config.toml

rm -rf "$tmp"
