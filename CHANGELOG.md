# Changelog

## Unreleased

### Added

- `sendit run -d` (`--detach`) runs the VM in the background: sendit returns once the VM answers over SSH, `sendit ssh` reaches it and `sendit stop` shuts it down, and closing the terminal doesn't. Ctrl-C before then, or the VM not answering within two minutes, shuts it down again. Its console output goes to `run/console.log` in the VM's directory.

### Changed

- sendit makes `~/.cache/sendit/` and the VM directory (`~/.sendit/` or `$SENDIT_VM_DIR`) accessible only to you, on every run, so directories made by earlier versions are fixed too. Other users of the Mac could read them before: the base images, the VM disks, and the copies of the provisioning scripts that a build keeps, which hold whatever the scripts put there, secrets included.

## 0.8.0 - 2026-10-17

### Added

- VMs swap to compressed memory (zram): up to half of their memory, compressed with zstd, which may itself take at most a quarter of it. A build that briefly needs more memory than the VM has slows down instead of being killed. Images get it when they are rebuilt with `sendit provision NAME --force`, and existing VMs when they are made again from such an image with `sendit reset`.
- `sendit status` tells when the project's VM was made from an earlier build of its image than the latest, and that `sendit reset` makes it again from the current one; for a worktree's VM that would be a copy of its main worktree's VM, also when that VM is, and that `sendit run --fresh` after the reset does. `sendit list` marks such VMs with "(earlier build)" after their image. Images record when they were built from now on, so this shows up for VMs made before the next build of their image.
- `sendit images --delete NAME` deletes a base image you no longer want, and the console log of its last build, also what a first build that failed left behind; repeat `--delete` for more images. The VMs made from it keep working; once one is reset, `sendit run` can't make it again from the image until `sendit provision NAME` builds it again. Its image file stays, and `sendit provision --all` builds the image again while it does or the config file names the image, and always for `default`; sendit says so after deleting it.
- An example provisioning script, `provision-scripts/claude-md.sh`, installs a `CLAUDE.md` that tells Claude Code about the VM: no sudo, the hidden `.git`, the host's name, and that the project runs on the Mac later. It goes to `/etc/claude-code/CLAUDE.md`, Claude Code's managed memory file, so a `~/.claude` shared from the Mac doesn't hide it and nothing in the VM can change it.

### Changed

- **Breaking:** each image with custom provisioning is defined by an image file, `~/.config/sendit/images/NAME.sh`. It is a bash script that calls the shared scripts in `~/.config/sendit/provision-scripts/` with `sendit-provision SCRIPT.sh`, in the order it needs them, and can run commands of its own in between. Shared scripts can call `sendit-provision` too, and each script runs at most once per build. The image file and the scripts run with `bash -e -o pipefail`, so a failing command, pipelines included, stops the script it's in, a failing script stops the build, and a script that fails also fails the build when the script that called it carries on, as does one still running when the image file ends, e.g. in the background. After each `sendit-provision`, the calling script reads `~/.profile` again, so its next commands see what the called script added, e.g. to the `PATH`. [`images/example.sh`](images/example.sh) is an example. An image file that is a symlink to nothing, e.g. into a dotfiles repository that isn't cloned yet, stops `sendit provision` instead of building the image without it, and `sendit status` and `sendit images` show an image built before the link was made as changed, not as provisioned; so does a mistake in the scripts, e.g. a call of a script that doesn't exist, a call with more than one script name, or a mount line sendit can't read.

  Scripts at the top level of `provision-scripts/` no longer run for every image, only when an image file calls them, and subfolders of `provision-scripts/` are no longer supported: sendit stops with an error while there is one. Without `images/default.sh`, the `default` image is the plain base image. To get the old behaviour back, move the scripts of each `provision-scripts/NAME/` to `provision-scripts/`, renaming those whose name a shared script has, and write `images/NAME.sh` to call them together with the shared scripts they didn't replace, in the order they ran before: by file name. For the `default` image, call the shared scripts from `images/default.sh`, e.g.:

  ```bash
  sendit-provision 00-rust.sh
  sendit-provision 01-helix.sh
  sendit-provision 10-claude-code.sh
  ```

  Images built with custom scripts show "custom scripts changed" once.
- **Breaking:** the directories that an image's scripts ask to share with `# sendit-mount:` lines are recorded when the image is built, and each VM keeps those of the build it was made from. Editing a mount line no longer changes the shares of existing VMs: it marks the image as changed, and takes effect with the next build, `sendit provision NAME --force`, for the VMs made after it; `sendit reset` remakes an existing one. A worktree's VM that is a copy of its main worktree's VM keeps that VM's, also when made again, unless `sendit run --fresh` makes it from the image. `sendit status` says when a VM shares what an earlier build asked for, and how to make it again from the current one. `run` and `status` no longer take shares from the scripts, nor fail on mistakes in them; `status` still tells when they changed. Images built before this share nothing for their scripts until they are rebuilt; VMs made before this share what the latest build of their image asked for.
- **Breaking:** building an image takes its CPUs and memory from the same places as the VMs made from it: the top-level keys, then its `[images.<name>]` table, then the flags of `sendit provision`. The `[provision]` and `[provision.<name>]` tables are gone; configs that have them must move their settings to `[images.<name>]`, or pass `--cpus` and `--memory` to `sendit provision` for a build that needs more.

### Removed

- sendit no longer reports its own status with the Program Status Protocol (OSC 7501), and no longer clears the records that programs in the VM left when it stops. The console still passes their reports through unchanged.

### Fixed

- `sendit run` also asks before sharing the target of a provisioning script or image file that is a symlink, e.g. into a dotfiles repository, even if the target doesn't exist yet, and of each symlink on the way to it, symlinked directories such as a linked `~/.config` included, also for sendit's own directories. Through them, the VM could change what runs with sudo in the next build of an image.
- `sendit run --worktree` in a subdirectory of a repository, as in a monorepo, boots the VM of the same subdirectory of the worktree instead of the worktree's root. That VM starts as a copy of the VM of the subdirectory in the main checkout, with its image, and uses its `[projects."<path>"]` table. A subdirectory of a linked worktree is recognized as one also without `--worktree`.

## 0.7.0 - 2026-10-09

### Added

- `sendit run <command>` boots the VM, runs the command over SSH once the VM answers, as `sendit ssh <command>` does, and shuts the VM down when it ends. sendit exits with the command's exit code, or with 128 plus the signal's number if `sendit stop`, Ctrl-C or a closed terminal stops the VM first. The console isn't shown then.
- `sendit run --worktree <branch>` checks out a branch in a new git worktree next to the main checkout, in `<repo>.worktrees/`, and boots its VM. `--from` sets where a new branch starts and `--worktree-path` where the worktree goes. For a branch that has a worktree already, it boots that one's VM.
- The VM of a linked git worktree starts as a copy of the main worktree's VM, if that has one and it is stopped, instead of the base image: with everything installed and cached there. While the main worktree's VM runs, `sendit run` asks whether to start from the base image instead.
- A linked git worktree without a `[projects."<path>"]` table of its own uses its main worktree's.
- `sendit status` shows which VM the project's VM is a copy of, or will be when `sendit run` creates it.

### Changed

- Commands given to `sendit ssh` run in a login shell, in the project directory, like an interactive session: they find what the provisioning scripts added to the PATH in `~/.profile`. In a terminal, they get one in the VM too, so interactive programs work.
- A boot removes the empty mount points of mounts the VM no longer has, e.g. after a mount was dropped from the config, or in a worktree's copy of its main worktree's VM, that VM's project directory.
- Answering no to one of sendit's questions, e.g. whether to share the home directory or to delete a VM, makes it exit with 1, as nothing was done.
- When the VM is already running, `sendit run` points to `sendit ssh`, or for `sendit run <command>`, to `sendit ssh <command>`, with `-C` if the project isn't the current directory.
- `sendit provision` rebuilds an image made by an older version of sendit without `--force`: VMs can't be made from it anymore. For an image whose custom scripts changed, it says so instead of only that the image is provisioned already.
- `sendit status` says that the VM was made from another image than the one chosen, or from an outdated base image, also while the VM runs.

### Fixed

- Two `sendit provision` running at once no longer break each other's images: the second one waits until the first is done. Building the same image twice at once could leave a base image behind that a provisioning VM was still writing to.
- `sendit stop` no longer signals whichever process now has the PID of the VM's last run while `sendit reset` or `prune` deletes the VM, or a git worktree's VM is copied from it.
- A crash or power cut during a VM's first boot can no longer leave its MAC address or machine identifier file empty, which kept the VM from starting again.
- `sendit run` reports that it failed to the terminal also when it fails before the VM starts, e.g. because the image isn't provisioned or the VM is already running.
- A failed `sendit provision` deletes the image it was building, which took up gigabytes in `~/.cache/sendit/images/` until the image was built again. The console log stays.
- A VM that fails to be created, e.g. for lack of space, no longer leaves a hidden partial copy in the VM directory that no command lists or deletes.

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
