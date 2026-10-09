# Changelog

## Unreleased

### Added

- `sendit run <command>` boots the VM, runs the command over SSH once the VM answers, as `sendit ssh <command>` does, and shuts the VM down when it ends. sendit exits with the command's exit code. The console isn't shown then.
- `sendit run --worktree <branch>` checks out a branch in a new git worktree next to the main checkout, in `<repo>.worktrees/`, and boots its VM. `--from` sets where a new branch starts and `--worktree-path` where the worktree goes. For a branch that has a worktree already, it boots that one's VM.
- The VM of a linked git worktree starts as a copy of the main worktree's VM, if that has one and it is stopped, instead of the base image: with everything installed and cached there. While the main worktree's VM runs, `sendit run` asks whether to start from the base image instead.
- A linked git worktree without a `[projects."<path>"]` table of its own uses its main worktree's.
- `sendit status` shows which VM the project's VM is a copy of, or will be when `sendit run` creates it.

### Changed

- Commands given to `sendit ssh` run in a login shell, in the project directory, like an interactive session: they find what the provisioning scripts added to the PATH in `~/.profile`. In a terminal, they get one in the VM too, so interactive programs work.
- When the VM is already running, `sendit run` points to `sendit ssh`.

## 0.6.0 - 2026-10-09

### Added

- `sendit run --read-only`, or `read-only = true` in the config file, shares the project directory read-only. With `--expose-git`, `.git` is read-only too.
- `SENDIT_VM_DIR` keeps the project VMs in another directory instead of `~/.sendit`, e.g. on an external drive. See the README for its limits.
- Programs in the VM can report their status to the terminal with the Program Status Protocol (OSC 7501). The console already passed the reports through; sendit now clears the records they left when the VM stops.
- sendit reports its own status with the Program Status Protocol too: building base images (and whether they are ready or failed), waiting for an answer, waiting for `sendit stop`, and `sendit run` ending with an error. Terminals that support it can show this e.g. in a tab that isn't in front.

### Changed

- `disk-size` in the config file must be at least 16 GiB, the size of a base image's disk, instead of 8 GiB. A VM's disk can't shrink below its base image's, so smaller sizes only got a warning on every run.
- `sendit status` and `sendit run` describe a base image that needs building in the same words, e.g. "not provisioned yet; run `sendit provision`".

### Fixed

- A Ctrl-C or other signal that ended one VM no longer stops the next one as soon as it starts, e.g. the next image's VM in `sendit provision --all`. Between VMs, Ctrl-C ends sendit right away again.
- `sendit list`, `images` and `prune` only take directories named like a VM for VMs, so `sendit prune --all` leaves other folders in the VM directory alone. VMs of projects whose folder name starts with a dot are no longer skipped.
- When an SSH key is missing, `sendit ssh` says that the images need rebuilding and the VM a reset, not just `sendit provision --force`.
- On boot, the VM checks that each share and the `.git` mask landed where they belong, and undoes them otherwise: something writing to a shared directory meanwhile, such as another VM, could have swapped a folder on the way for a symlink.
- Mount points that the VM creates in the user's home are created as the user instead of by root and handed over, so a symlink swapped in on the way can't leave a folder owned by the user elsewhere, e.g. in `/etc`.
- `sendit provision` no longer hangs after a custom script ran out of memory. The out-of-memory killer took cloud-init down too, which then never powered the VM off; it now powers off on its own.

### Base image

The base revision is now 10: rebuild images from 0.5.0 with `sendit provision --all --force`, then delete their VMs with `sendit prune --outdated`. Until then, sendit can't make VMs from the old images or run the VMs made from them.

- Base images boot Debian's kernel with 16 KiB pages (`linux-image-arm64-16k`) instead of the cloud kernel with 4 KiB pages. Under memory pressure on the Mac, VMs running the cloud kernel lost writes to their memory, which crashed programs and the kernel and showed up as I/O errors; so far, none running the 16 KiB kernel have.
- Base images get a 16 GiB disk instead of 8 GiB, which leaves custom provisioning scripts more room.
- A root partition that fails to grow shows up as a failed `sendit-growfs` service instead of being ignored.
- The shares are mounted before cron and atd start, should a custom script install them, so that their jobs can't plant symlinks while the shares are mounted as root.

## 0.5.0 - 2026-10-02

### Added

- Custom provisioning scripts can ask for host directories to be shared with the VMs made from their image, with `# sendit-mount: HOST[:GUEST][:ro|rw]` lines in the comments at their top. A directory that doesn't exist on the Mac is left out with a notice.

### Changed

- Changing the comments at the top of a custom provisioning script, before its first command, no longer marks the images built with it as changed. Images built with custom scripts by earlier versions show up as changed once; `sendit provision NAME --force` rebuilds them.
- sendit excludes `~/.cache/sendit/` and `~/.sendit/` from Time Machine backups. Both hold only downloads, base images and VMs that sendit can make again, and every run of a VM made Time Machine copy its whole disk again. Files in a VM outside the project directory are no longer backed up.

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
