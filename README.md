Hi and thanks for checking out this project! First things first, this is completely vibe coded (except these two paragraphs). I still haven't made up my mind yet if I like or despise LLMs, but I believe it is a must for every developer to at least follow that space a little bit to stay up-to-date and, ideally, make their own experiences away from all the omnipotent hype that grasps every LLM community in existence (it seems).

I let Claude Code build this project based on my experiences with [vibe](https://github.com/lynaghk/vibe) and I truly want to thank the maintainer for putting it together. `vibe` is a very opinionated project (which I like!) and it served me well on the occasions when I would spin up an LLM (and believe me when I say these LLMs never got to see my production system). I just had a couple of ideas that wouldn't fit `vibe` very well, so I decided to gain some more experience with LLMs and create "Send it".

~SwishSwushPow

# Send it

Send it (`sendit`) gives every project its own light and fast Debian VM on macOS, built on Virtualization.framework, for running things like coding agents against a project without giving them the rest of the machine.

You provision a Debian base image once, or several with different tools. Each project's VM starts as an APFS clone of one: instant, and taking no space until the guest writes. The project directory is shared read-write, with `.git` hidden behind an empty read-only mount so the VM can't touch the history, as long as nothing in it becomes root (see [Limits](#limits)). The VM's user has no sudo rights; only the host can become root.

## Building

Requires macOS 13 or later on Apple silicon (tested on 15 and 27). Install with `cargo install send_it_vm`, or `cargo install --path .` from a clone.

Virtualization.framework only runs binaries signed with the virtualization entitlement. Builds from a clone are signed at link time by `scripts/link-and-sign.sh`; a binary installed from crates.io signs itself (ad hoc) on its first run and starts again.

## Usage

```sh
sendit provision          # download Debian and build this project's base image (once)
sendit run                # boot this project's VM and attach to its console
sendit run --worktree x   # ... of a git worktree on branch x, made if needed
sendit run cargo test     # boot the VM, run a command in it, then shut it down
sendit run -d             # boot the VM in the background
sendit attach             # attach to the console of the VM in the background
sendit ssh                # open another shell in the running VM
sendit ssh cargo test     # ... or run a command there
sendit ssh --root         # ... as root, e.g. to install packages
sendit stop               # shut the running VM down
sendit status             # show the VM and its effective settings
sendit reset              # delete the VM; the next run starts from a fresh copy
sendit list               # list all project VMs
sendit images             # list the base images and how many VMs use each
sendit images --delete go # delete the go image; its VMs keep working
sendit prune              # delete VMs whose project directory is gone
sendit prune --outdated   # ... and VMs made from an outdated base image
sendit prune --all        # delete every VM that isn't running
```

Commands act on the project in the current directory, or on `-C DIR`. `reset`, `prune` and `images --delete` ask first; `-y`/`--yes` skips that, and is needed without a terminal.

A command given to `sendit run` or `sendit ssh` runs as the VM's user (or root, with `sendit ssh --root`) in a login shell in the project directory, so it gets the `PATH` your provisioning scripts set in `~/.profile`. Arguments arrive unchanged; use `sh -c '...'` for pipes and the like. Options before the command are sendit's, so a mistyped one is an error instead of the command; a command that starts with `-` goes after `--`. It gets a terminal when sendit has one, so `sendit run claude` works, and its output can be piped otherwise. sendit exits with the command's exit code, or with 128 plus the signal's number if the VM stops first, e.g. through `sendit stop` or Ctrl-C. With a command, `sendit run` skips the console, waits for SSH, and shuts the VM down when the command ends.

`sendit run -d` (`--detach`) runs the VM in the background, e.g. for a server in it that you reach from the Mac. sendit returns once the VM answers over SSH; then `sendit ssh` reaches it and `sendit stop` shuts it down, and closing the terminal doesn't. If you press Ctrl-C before that, or the VM doesn't answer within two minutes, sendit shuts it down again. Its console output goes to `run/console.log` in the VM's directory, and `sendit attach` shows its console in your terminal: every key goes to the VM, Ctrl-C included, and closing the terminal detaches it again while the VM keeps running. One terminal can be attached at a time. As in the foreground, `exit` in the console shuts the VM down. A VM in the background keeps all the memory it has used until it stops; `sendit list` shows which ones are still running.

`sendit ssh` ignores `~/.ssh/config`, so settings like `ForwardAgent yes` under `Host *` don't hand your SSH agent to the VM.

Inside the VM, the host is `host.sendit.internal`. Host services must listen on more than `127.0.0.1` to be reachable.

In the console, `exit` or Ctrl-D shuts the VM down. Ctrl-] asks the guest to shut down, and a second press forces it off. Any shutdown that takes over 30 seconds, however it started, is forced too.

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes:

- `--cpus N` and `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, read-only unless `:rw`, at `/mnt/<name>` without `GUEST`. Repeatable. A dropped mount's empty mount point is removed on the next boot. Think twice before `:rw`: the VM can then change files the Mac runs later, such as a crate's `build.rs` in `~/.cargo/registry`.
- `--read-only`: shares the project read-only. Builds that write into it, such as `target/` or `node_modules/`, must be pointed elsewhere.
- `--expose-git`: lets the VM see and change `.git`, including the hooks and config git on the Mac runs.
- `--image NAME`: the base image to create the VM from. Later runs keep using it.

`run` asks before sharing a directory, read-only or not (reading the root SSH key is enough), that contains your home directory, or that contains or sits inside sendit's own files (`~/.config/sendit`, `~/.cache/sendit`, `~/.sendit` or `$SENDIT_VM_DIR`), following symlinks, e.g. a provisioning script linked from a dotfiles repository: through those the VM could become root or run commands on the Mac. It checks when it sets up the shares, so a symlink made while a VM runs isn't caught until that VM's next run. sendit can't trace hard links, so link scripts and image files with symlinks.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, `[images.<name>]` overrides CPUs and memory for an image and its VMs, and `[projects."<path>"]` overrides both for one project. Flags override everything; mounts from all layers are combined.

```toml
cpus = 4                  # default 2
memory = "8G"             # default 4G
disk-size = "128G"        # default 64G; the disk file is sparse and only grows
mounts = ["~/.cargo/registry:/home/dev/.cargo/registry"]  # read-only unless :rw
banner = false            # leave the "Send It" banner out of the login message

image = "rust"            # the base image new VMs are made from; default "default"

[images.rust]             # CPUs and memory for building one image and its VMs
cpus = 6

[projects."~/Code/app"]
expose-git = true

[projects."~/Code/vendor"]
read-only = true          # share the project directory read-only
```

The VM swaps to compressed memory (zram) of up to half its `memory`, so a build that briefly needs more memory than the VM has slows down instead of being killed. An image file can turn that off with `sudo ln -sf /dev/null /etc/systemd/zram-generator.conf`, or retune it with a drop-in in `/etc/systemd/zram-generator.conf.d/`.

## Base images

New VMs are made from the `default` image unless `image` or `--image` picks another, and keep using the image they were made from. If that setting later names a different image, `sendit run` refuses to start the VM until `sendit reset` deletes it. Rebuilding or deleting an image doesn't affect existing VMs.

`sendit provision NAME` builds image `NAME`; without a name, the current project's image, and with `--all`, every image sendit knows of; `--cpus` and `--memory` override what the build gets. Names are up to 64 lowercase letters, digits and dashes, not starting with a dash.

### Image files

`~/.config/sendit/images/NAME.sh` customizes image `NAME` and runs at the end of provisioning; without `default.sh`, the `default` image is the plain base image. It's a bash script that calls shared scripts from `~/.config/sendit/provision-scripts/` with `sendit-provision`:

```bash
sendit-provision rust.sh
sendit-provision helix.sh    # calls rust.sh too, which runs only once
sudo apt-get install -y postgresql-client
sendit-provision claude-code.sh
```

The image file sets the order, and can run its own commands in between or call a script only `if` something holds. Scripts can call `sendit-provision` too; each runs at most once per build, so several can share one they all need. The image file and the scripts run with `bash -e -o pipefail`: a failing command, pipelines included, stops the script it's in, and a failing script stops the build. A failed script fails the build even if the script that called it carries on, e.g. with `sendit-provision x.sh || true`. So does one that is still running when the image file ends, e.g. one started in the background with `&` and never waited for, and a call of a script that is still running fails.

sendit finds the scripts an image needs by reading the `sendit-provision NAME` calls, skipping comments, quotes and here-documents (a `$(...)` in double quotes counts), so `NAME` must be a file name in `provision-scripts/` written out literally, and the only argument. The command must be the bare word `sendit-provision` too, not run through `eval`, `bash -c` or a full path, and that word anywhere outside quotes and comments counts as a call, so quote it in e.g. `command -v 'sendit-provision'`. `sendit provision` refuses names it can't find, scripts that call each other in a cycle, mount lines it can't read, and image files that are dangling symlinks (symlinks into e.g. a dotfiles repository are fine). `run` takes no shares from the scripts and doesn't fail on mistakes in them, so existing VMs keep working while you edit; it only stops while `provision-scripts/` has subfolders.

Each script runs as the VM's user in its own login shell from the home directory, as `bash FILE`, so its shebang line is ignored. `~/.profile` is read again after each `sendit-provision`, so later commands see what earlier scripts `export` in it, e.g. `PATH`. sudo is available only while provisioning. Trust the scripts, and whatever they download, as you would root in every VM made from the image: sendit takes sudo away afterwards, but not a setuid binary or root service they left behind.

`sendit status` and `sendit images` tell you when an image's scripts changed since it was built, apart from their leading comments other than mount lines; `sendit provision NAME --force` rebuilds it. `sendit status` also tells you when the project's VM was made from an earlier build of its image, and how to make it again from the latest one: `sendit reset`, then for a worktree's VM that would be a copy of a main VM from an earlier build too, `sendit run --fresh`; `sendit list` marks such VMs with "(earlier build)".

A script can ask for host directories to be shared with the image's VMs, in comment lines before its first command:

```bash
#!/bin/bash
# sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry
```

These take the same form as `--mount`, with host paths absolute or starting with `~`, and come after the top-level mounts and before the project's. A host directory that doesn't exist is left out with a notice. A script's mounts apply to every image that calls it, even from an `if` that doesn't run. They are recorded when the image is built, and each VM keeps those of the build it was made from: a changed mount line takes effect with the next build, for the VMs made after it, so rebuild the image, then `sendit reset` the VMs that should have it. A worktree's VM that is a copy of its main worktree's VM keeps that VM's, also when made again; `sendit run --fresh` makes it from the image instead. `sendit provision` lists them and `sendit status` shows them. Read the mount lines of a script you copy: they give the VM access to files on the Mac.

[`provision-scripts/`](provision-scripts) has example scripts for Rust, the Helix editor, Claude Code, the Pi coding agent, and a `CLAUDE.md` that tells Claude Code about the VM (installed as `/etc/claude-code/CLAUDE.md`, so a `~/.claude` shared from the Mac doesn't hide it). [`images/example.sh`](images/example.sh) calls most of them. Copy what you want to `~/.config/sendit/provision-scripts/` and `~/.config/sendit/images/`.

### Git worktrees

`sendit run --worktree <branch>` checks the branch out in a worktree and boots that worktree's VM. The worktree goes to `<repo>.worktrees/<branch>`, with `/` replaced by `-` (e.g. `~/dev/repo.worktrees/feature-x` for `feature/x`), or to `--worktree-path`. A new branch starts at `HEAD` or `--from`; a branch only a remote has is tracked as with `git checkout`, and `--from <remote>/<branch>` picks among several remotes. If the branch has a worktree already, its VM boots, so the same command takes you back later. For a project in a subdirectory of the repository, it boots the VM of the same subdirectory of the worktree. Your shell's directory doesn't change, but the console starts in the worktree.

Any linked worktree gets its own VM. If the main worktree has a stopped VM made from the same image, the worktree's VM starts as an instant copy of it, with everything installed, logged into and cached there, and the same SSH host keys and machine ID. A copy of a running VM could miss unwritten data, so while the main VM runs, `sendit run` asks whether to use the base image instead (default: no, stop the main VM first). `--fresh` starts from the base image, the main VM's unless `--image` or the config names another; for a worktree whose VM is a copy already, `sendit reset` it first. A worktree also uses its main worktree's `[projects."<path>"]` table unless it has its own. Subdirectories map to the same subdirectory of the main worktree for both. `sendit status` shows which VM a worktree's VM is, or will be, a copy of.

## Limits

The VM is a boundary, but not one that makes it safe to run anything in it and then trust the project folder:

- Hiding `.git` relies on root in the VM staying out of reach. The read-only mount is made inside the VM, so anything that gets root there can remove it and change the history, hooks or config that git on the Mac runs. Root could come from a kernel exploit, something a provisioning script left behind, or a package installed with `sendit ssh --root`.
- Only the top-level `.git` is hidden, and only if it exists at boot. The VM can plant a repository elsewhere: a `.git` in a subfolder, a submodule's `.git` swapped for its own, or a top-level one in a project that had none. Its config can run a command, e.g. through `core.fsmonitor`, as soon as git on the Mac looks inside: a `git status` in the project root is enough for a submodule, as is a shell prompt or editor showing git status in a subfolder.
- More generally, anything the VM changes may run on the Mac later: `build.rs`, `.cargo/config.toml`, Makefiles, `package.json` scripts, `.vscode/tasks.json`, test suites. Review the changes before building or running them on the Mac.
- Console output and `sendit ssh` sessions go straight to your terminal, escape sequences included. Depending on the terminal, these can set its title or, through OSC 52, the clipboard, so a later paste into a shell runs what the VM chose. Check your terminal's clipboard settings.
- The VM has network access: the internet, the local network, and every service on the Mac that listens on more than `127.0.0.1`, such as Remote Login or File Sharing.

## Where things live

Nothing is written into project directories.

- `~/.cache/sendit/`: Debian downloads, the base images (in `images/`), and SSH keys
- `~/.sendit/<project>_<uuid>/`: one directory per project VM
- `~/.config/sendit/`: configuration, image files (in `images/`) and the provisioning scripts they call (in `provision-scripts/`)

sendit excludes `~/.cache/sendit/` and the VM directory from Time Machine backups: they hold only what sendit can make again, and a VM's disk changes on every run. Keep anything you can't lose in the project directory. It also makes both accessible only to you, since base images, VM disks and the files of a build hold whatever the provisioning scripts put there, secrets included.

To keep the VMs elsewhere, e.g. on an external drive, set `SENDIT_VM_DIR` to an existing directory. sendit uses it in place of `~/.sendit` and won't create it, so a drive that isn't connected is an error. It can't be the root of a volume, your home directory or a folder containing it, and it can't overlap `~/.cache/sendit` or `~/.config/sendit`. Folders in it that aren't VMs are left alone, by `sendit prune --all` too. Things to know about other drives:

- A VM's disk is a copy of the base image instead of an instant clone, so each VM takes the base image's space on top of its own changes.
- Format the drive as APFS: HFS+ and exFAT have no sparse files, so every VM takes its full disk size, 64 GiB by default.
- If the drive ignores ownership, as external drives often do, other users of the Mac can change the VMs' files and so run commands as root in them.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
