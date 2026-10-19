#!/bin/bash
# Installs REA (Reverse Engineer Anything, github.com/morluto/rea), an MCP
# server for reverse engineering, with Ghidra as its engine for native
# binaries, and registers it with Claude Code: `rea setup` adds it to
# ~/.claude.json and installs its skill in ~/.claude/skills. The version is
# pinned because each REA release accepts only one Ghidra release line
# (12.1.x for this one).

sendit-provision node.sh
sendit-provision ghidra.sh
sendit-provision claude-code.sh

version=6.7.0

# REA checks process ownership with procps' ps before starting Ghidra.
command -v ps > /dev/null || sudo apt-get -y install --no-install-recommends procps

sudo npm install --global --no-fund --no-audit "rea-agents@$version"
sudo rm -rf /root/.npm

# GHIDRA_INSTALL_DIR and JAVA_HOME come from ~/.profile; setup checks the
# installation and records both in the registration.
rea setup --client claude_code --yes
rea doctor --provider ghidra --json > /dev/null
