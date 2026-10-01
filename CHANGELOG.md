# Changelog

## Unreleased

### Fixed

- VMs use the host's time zone instead of UTC. sendit passes it on every boot, so existing VMs pick it up without rebuilding their image.

## 0.4.0 - 2026-09-30

### Added

- `sendit prune --outdated` also deletes VMs made from an outdated base image.
- `sendit prune --all` deletes every VM that isn't running.
- `banner = false` in the config file, or in a project's table, leaves the "Send It" banner out of the login message. The message now comes from the host on every boot, so existing VMs pick up the setting without rebuilding their image.

### Fixed

- A shutdown that the VM started itself, such as after `exit`, is forced off after 30 seconds, like one that sendit asked for.
- Logging out of the console no longer prints "The system will power off now!" after the shutdown message.
- Starting a VM scrolls earlier terminal output into the scrollback instead of clearing it.

### Base image

The base revision is now 9: rebuild images from 0.3.1 with `sendit provision --all --force`, then delete their VMs with `sendit prune --outdated`.

- Removed unattended-upgrades and the apt-daily timers. Upgrades running in the background could delay shutting down by up to 30 minutes; updates now come with a rebuilt image.
- Added cmake, unzip, jq, ripgrep, fd and htop.
- `sendit ssh --root` gets a coloured shell with a red prompt.

## 0.3.1 - 2026-09-28

### Fixed

- After provisioning, the cursor no longer jumps back up, letting the next image's output overwrite what was shown. sendit leaves the alternate screen only if the guest entered it.
- `sendit provision --all` no longer swallows the first key pressed for the next image's VM, e.g. a Ctrl-] to stop it.
- A project VM's console starts on a clean screen again.
- Provisioning output stops at the build's result line; what the guest prints while shutting down afterwards only goes to the console log, so a failing script's message stays in view.
- The Debian tarball is deleted once it is unpacked, saving a few hundred MB of cache.
- `sendit provision --all` no longer leaks a thread and a pipe per image.
- Custom provisioning scripts are listed one per line, with the image's folder named once.

## 0.3.0 - 2026-09-27

### Changed

- **Breaking:** extra mounts are read-only unless `:rw` is given. Configs that relied on the read-write default must add `:rw`.
- `sendit run` asks before any share that contains the home directory or sendit's own files, read-only ones included. Through those the VM could become root or add shares for the next run.

### Fixed

- `sendit ssh` no longer reads `~/.ssh/config`, so settings meant for trusted hosts, like `ForwardAgent yes`, no longer apply to the VM.
- A provisioning build only passes on its own success line, which ends in a token sendit makes up for each build. Before, a custom script printing the success marker could let a failed build through.
- The readme has a Limits section on what the VM can still do to the Mac.

## 0.2.0 - 2026-09-26

### Changed

- **Breaking:** `[images.<name>]` now sets the CPUs and memory of the VMs made from an image. Resources for building an image move to `[provision.<name>]`, so configs that used `[images.<name>]` for building must rename it.

### Added

- Inside the VM, `host.sendit.internal` points at the host, whatever subnet macOS gives its VMs.

### Fixed

- The first command line in the console is no longer garbled by a late reply to systemd's terminal size query.
- The readme gives the right minimum macOS version, 13.

### Base image

The base revision is now 8: rebuild images from 0.1.0 with `sendit provision --all --force`, then reset their VMs.

## 0.1.0 - 2026-09-25

First release.

- Each project gets its own Debian VM on Virtualization.framework, created as an APFS clone of a base image built once with cloud-init.
- The project is shared into the VM over virtiofs at `/home/dev/<name>`, with its `.git` hidden, plus extra mounts from the config file or `--mount`.
- Commands: `provision`, `run`, `ssh`, `stop`, `reset`, `status`, `list`, `images` and `prune`.
- Named base images, each with its own custom provisioning scripts, chosen per project.
- CPUs, memory and disk size from the config file, globally or per project; CPUs and memory also on the command line, and for building each image.
- The console follows the host terminal's size and type; logging out of it shuts the VM down.
- The VM's user has no sudo rights; only the host becomes root, with `sendit ssh --root`.
- Example provisioning scripts for Rust, Helix, Claude Code and Pi.
