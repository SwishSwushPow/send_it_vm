#!/bin/bash
# Installs Claude Code with the native installer, into ~/.local/bin. Debian's
# ~/.profile adds that to the PATH in login shells once it exists, so the
# console and `sendit ssh` find `claude` right away.
set -euo pipefail

curl -fsSL https://claude.ai/install.sh | bash
