# sendit-ui

A desktop app for [sendit](../README.md), built with [GPUI](https://gpui.rs) and [GPUI Kit](https://gpui-kit.com). It shows your project VMs, base images and configuration, and does what the `sendit` commands do, apart from the interactive sessions, which open in your terminal app.

## Building

sendit-ui is a member of sendit's Cargo workspace and shares its `Cargo.lock` and `target/`; a plain `cargo build` or `cargo test` in the repository's root builds sendit alone, `-p sendit-ui` or `--workspace` builds the app. It needs macOS on Apple silicon, like sendit, and sendit itself, e.g. from `cargo install --path ../sendit-vm` in this folder.

```sh
cargo run --release      # run it from here
./bundle.sh              # or build ../target/Send it.app, to start from the Finder
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
- **Activity**: the sendit commands the app ran, with their output and log file, and Cancel for those that run.
- **Preferences**: light or dark, or as macOS is set (the default), which the app follows when macOS switches; the terminal app and editor to open things in; where sendit and the VM folder are.

## How it works

The app reads VMs, images and the config file through sendit's library (`send_it_vm`), every few seconds, off the main thread. It runs the `sendit` binary for everything that changes them, so locks, signing and background VMs work exactly as on the command line. Questions sendit would ask on a terminal, the app asks first, then passes `--yes`. These are: deleting things, sharing directories that let the VM get around its limits, and starting a worktree's VM from the image while its main worktree's VM runs. For the shares and the worktree, sendit refuses without a terminal and says why; the app shows that and starts again with `--yes` or `--fresh` once you agree. Cleaning up deletes only if the VMs to delete are still the ones listed.

sendit's output goes to a log file per operation, in `~/Library/Caches/sendit-ui/operations`, which the app shows as it grows. Logs older than a week are deleted. So what the app started goes on if the app quits, e.g. a build. Quitting while something runs asks first; to stop it instead, cancel it in Activity, which interrupts it as Ctrl-C would. VMs started in the background keep running in any case.

## Development

`cargo test` covers the parts without a window: the run options, the config editor, the operations and the terminal launching. To look at the window without a screen to capture, build with the `screenshot` feature:

```sh
SENDIT_UI_SCREENSHOT=/tmp/vms.png SENDIT_UI_SELECT=my-app cargo run --features screenshot
```

It saves the window after a few seconds and quits. `SENDIT_UI_PAGE` picks the page (`images`, `config`, `maintenance`, `activity`, `preferences`), `SENDIT_UI_PANEL` a VM's panel (`start`, `worktree`, `command`, `rename`), `SENDIT_UI_ACTION` does what a button does (`build:IMAGE`, `start-dir:DIR`, `stop:NAME`), `SENDIT_UI_APPEARANCE` shows it `light` or `dark` without saving that, and `SENDIT_UI_WAIT` sets the seconds to wait before saving.

The app icon is the S of the banner a VM shows when it starts, light or dark. `icon/make-icon.swift` draws it, and `icon/AppIcon.icon` is the Icon Composer document that macOS 26 and later show it from, in the Finder and the Dock, light, dark or tinted as set in System Settings. After changing either, run `icon/make-icon.sh`, which needs Xcode. It writes the document's layers and compiles the document with `actool` into `icon/Assets.car`, which macOS shows the bundle's icon from (flat before macOS 26), and `icon/AppIcon.icns`, a fallback for whatever reads icons the old way. It also writes the PNGs for the sidebar and, when the app runs outside its bundle, the Dock, which follow the app's appearance. It changes none of these unless all of it works.
