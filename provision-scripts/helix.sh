#!/bin/bash
# Builds the Helix editor from source with cargo, into ~/.cargo/bin, and
# puts its runtime files and config in ~/.config/helix.

sendit-provision rust.sh

git clone https://github.com/helix-editor/helix.git
cd helix
git checkout a05c151

# install helix
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo install --path helix-term --locked

# copy over remaining runtime files
mkdir -p ~/.config/helix
cp -r runtime/ ~/.config/helix/runtime/

# manually build grammars because there is an issue with go on this version
echo 'use-grammars = { except = [ "go", "gomod", "gotmpl", "gowork" ] }' >> ~/.config/helix/languages.toml
hx --grammar fetch
hx --grammar build

# set default theme
echo 'theme = "onedark"' >> ~/.config/helix/config.toml

# remove downloaded repo
cd ..
rm -rf helix

# remove tree sitter grammar sources
rm -rf ~/.config/helix/runtime/grammars/sources

# remove specific toolchain from this install
rustup toolchain uninstall 1.82.0
