#!/bin/bash
# Tells Claude Code about the VM it runs in: no sudo, the hidden .git, and
# that the project runs on the Mac later. Installed as the managed memory
# file, which Claude Code loads in addition to the user's and the project's
# CLAUDE.md. It lives outside the home directory, so a ~/.claude shared from
# the Mac doesn't hide it, and belongs to root, so the agent can't change it.

tmp=$(mktemp)
cat > "$tmp" <<'EOF'
# Environment: sendit VM

You are running inside a sendit VM: a disposable Debian 13 (arm64) virtual
machine on the user's Mac, made for running coding agents against one project.

## The machine
- The project is shared from the Mac at `/home/dev/<project>`. Everything
  outside it lives on the VM's disk and is lost when the user resets the VM.
- You are user `dev` with no sudo and no way to become root. Don't try
  `sudo` or `apt-get install`. If a system package is missing, tell the user
  which one; they can install it with `sendit ssh --root` on the Mac. User-level
  installs (`~/.local/bin`, `cargo install`, `uv tool`, `npm --prefix`) work.
- Installed in the base image: git, curl, build-essential (gcc, make),
  pkg-config, cmake, jq, ripgrep (`rg`), fd, unzip, less, vim-tiny, htop.
  Further tools from provisioning scripts are on the PATH of login shells;
  check with `command -v` before assuming one is missing.
- Architecture is linux/arm64 (aarch64). Pick matching downloads.
- Internet access usually works, but the local network, the Mac and other
  VMs are blocked on purpose, unless the user allowed them (`network` and
  `network-allow` in sendit's config); a blocked TCP connection fails with
  "Connection refused". Don't look for a way around it; tell the user what
  you need to reach. `localhost` is this VM; the Mac is
  `host.sendit.internal`, reachable only where allowed, and only for services
  listening beyond 127.0.0.1.
- Extra host directories may be mounted under `/mnt/` or elsewhere, usually
  read-only.

## Git
- The project's `.git` is normally hidden on purpose: git reports "not a git
  repository" or an empty `.git`. Don't work around it. Never run `git init`,
  and never create `.git` directories, submodules or nested repositories
  anywhere in the project. Leave commits to the user and summarize your
  changes instead.
- If git does work here, don't modify `.git/config` or hooks.

## Changing the project
- The user will build and run this project on their Mac. Don't change build
  scripts, package-manager scripts, CI config, editor task files or git-related
  files unless the task needs it, and point out any such change explicitly.
- Dependencies built here are Linux/arm64 builds. Say so when you install them
  into the project (e.g. `node_modules`, a `.venv`).
- If writes to the project fail with "Read-only file system", the user shared
  it read-only on purpose. Don't look for a way around it; put build output
  elsewhere (e.g. `CARGO_TARGET_DIR=~/target`).
EOF
sudo install -D -o root -g root -m 644 "$tmp" /etc/claude-code/CLAUDE.md
rm -f "$tmp"
