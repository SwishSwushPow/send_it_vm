# sendit-ui

A desktop app for [sendit](../README.md), built with [GPUI](https://gpui.rs) and [GPUI Kit](https://gpui-kit.com). It shows your project VMs, base images and configuration, and does what the `sendit` commands do, apart from the interactive sessions, which open in your terminal app.

## Building

sendit-ui is a Cargo workspace of its own, so that GPUI's dependencies stay out of sendit's. It needs macOS on Apple silicon, like sendit, and sendit itself, e.g. from `cargo install --path ..` in this folder.

```sh
cargo run --release      # run it from here
./bundle.sh              # or build target/Send it.app, to start from the Finder
```

It looks for `sendit` next to itself, in `~/.cargo/bin`, on `PATH` and in Homebrew's folders; Preferences can name another. An app started from the Finder doesn't see what your shell's profile sets, so if you set `SENDIT_VM_DIR` there, set the VM folder in Preferences too.

## What it does

- **VMs**: every project VM with its state, image and disk use, and for the selected one, what `sendit status` shows.
  - **Start** runs `sendit run -d`, also with options: CPUs, memory, image, network, extra shares, a read-only project, `.git` exposed, and for git worktrees, a branch to check out and boot (`--worktree`).
  - **Stop**, **Rename** and **Reset**.
  - **Shell**, **Root shell**, **Console** and **Run command** open in your terminal app: Ghostty, Terminal, or another set in Preferences.
  - **Console log**, **Show project**, and the `ssh sendit-<name>` command to copy.
  - **New VM** picks a project folder and starts its VM.
- **Images**: each base image's state, disk use and number of VMs. Build or rebuild them, with CPUs and memory for the build, delete them, and create or edit their image files.
- **Configuration**: a form over `~/.config/sendit/config.toml`. It covers the settings for all projects, per image and per project. It changes only what you change, so comments and layout stay, and checks the file the way sendit reads it before saving. A config file that is a link, e.g. into a dotfiles repository, stays one.
- **Maintenance**: clean up VMs as `sendit prune` does, with a preview; set up SSH host names (`sendit ssh-config`); see where sendit keeps things.
- **Activity**: the sendit commands the app ran, with their output, and Cancel for those that run.

## How it works

The app reads VMs, images and the config file through sendit's library (`send_it_vm`), every few seconds, off the main thread. It runs the `sendit` binary for everything that changes them, so locks, signing and background VMs work exactly as on the command line. Questions sendit would ask on a terminal, the app asks first, then passes `--yes`. These are: deleting things, sharing directories that let the VM get around its limits, and starting a worktree's VM from the image while its main worktree's VM runs.

Quitting while something runs asks first, then interrupts it as Ctrl-C would and waits for it to stop. VMs started in the background keep running.

## Development

`cargo test` covers the parts without a window: the run options, the config editor, the operations and the terminal launching. To look at the window without a screen to capture, build with the `screenshot` feature:

```sh
SENDIT_UI_SCREENSHOT=/tmp/vms.png SENDIT_UI_SELECT=my-app cargo run --features screenshot
```

It saves the window after a few seconds and quits. `SENDIT_UI_PAGE` picks the page (`images`, `config`, `maintenance`, `activity`, `preferences`), `SENDIT_UI_PANEL` a VM's panel (`start`, `worktree`, `command`, `rename`), `SENDIT_UI_ACTION` does what a button does (`build:IMAGE`, `start-dir:DIR`, `stop:NAME`), and `SENDIT_UI_WAIT` sets the seconds to wait before saving.
