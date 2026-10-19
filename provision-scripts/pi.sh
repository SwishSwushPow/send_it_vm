#!/bin/bash
# Installs the Pi coding agent (pi.dev). Pi needs Node.js 22.19 or newer,
# which Debian doesn't ship, so node.sh installs the current Node.js LTS
# first; pi's installer only offers to install Node.js itself when it can ask
# on a terminal. Without a terminal it installs without asking, into
# ~/.local/bin, which Debian's ~/.profile puts on the PATH.

sendit-provision node.sh

mkdir -p ~/.local/bin
export PATH="$HOME/.local/bin:$PATH"
curl -fsSL https://pi.dev/install.sh | sh
