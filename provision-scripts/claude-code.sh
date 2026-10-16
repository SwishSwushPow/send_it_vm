#!/bin/bash
# Installs Claude Code with the native installer, into ~/.local/bin. Debian's
# ~/.profile adds that to the PATH once it exists, so the console,
# `sendit ssh` and the commands after `sendit-provision claude-code.sh` find
# `claude`.

curl -fsSL https://claude.ai/install.sh | bash
