Hi and thanks for checking out this project! First things first, this is completely vibe coded (except these two paragraphs). I still haven't made up my mind yet if I like or despise LLMs, but I believe it is a must for every developer to at least follow that space a little bit to stay up-to-date and, ideally, make their own experiences away from all the omnipotent hype that grasps every LLM community in existence (it seems).

I let Claude Code build this project based on my experiences with [vibe](https://github.com/lynaghk/vibe) and I truly want to thank the maintainer for putting it together. `vibe` is a very opinionated project (which I like!) and it served me well on the occasions when I would spin up an LLM (and believe me when I say these LLMs never got to see my production system). I just had a couple of ideas that wouldn't fit `vibe` very well, so I decided to gain some more experience with LLMs and create "Send it".

~SwishSwushPow

# Send it

Send it (`sendit`) gives every project its own light and fast Debian VM on macOS, built on Virtualization.framework. It's meant for running things like coding agents against a project without giving them the rest of the machine.

A Debian base image is provisioned once, or several with different tools installed. Each project's VM starts as an APFS clone of one, so creating one is instant and takes no space until the guest writes. The project directory is shared into the VM read-write, and its `.git` is hidden behind an empty read-only mount, so the VM can't see or change the history, as long as nothing in it becomes root (see [Limits](#limits)). The VM's user has no sudo rights; only the host can become root.

## Building

Requires macOS 13 (Ventura) or later on Apple silicon; tested on macOS 15 (Sequoia) and 27. Install with `cargo install send_it_vm`, or `cargo install --path .` from a clone.

Virtualization.framework only works for binaries signed with the virtualization entitlement. `cargo build` and `cargo install --path .` link through `scripts/link-and-sign.sh`, which signs the binary. A binary installed from crates.io is built without it, so on its first run `sendit` signs itself (ad hoc) and starts again.

## Usage

```sh
sendit provision          # download Debian and build this project's base image (once)
sendit run                # boot this project's VM and attach to its console
sendit run --worktree x   # ... of a git worktree on branch x, made if needed
sendit run cargo test     # boot the VM, run a command in it, then shut it down
sendit ssh                # open another shell in the running VM
sendit ssh cargo test     # ... or run a command there
sendit ssh --root         # ... as root, e.g. to install packages
sendit stop               # shut the running VM down
sendit status             # show the VM and its effective settings
sendit reset              # delete the VM; the next run starts from a fresh copy
sendit list               # list all project VMs
sendit images             # list the base images and how many VMs use each
sendit prune              # delete VMs whose project directory is gone
sendit prune --outdated   # ... and VMs made from an outdated base image
sendit prune --all        # delete every VM that isn't running
```

Commands act on the project in the current directory, or on the one given with `-C DIR`.

A command after `sendit run` or `sendit ssh` runs the same way: as the VM's user (or root, with `sendit ssh --root`), in a login shell, so with the PATH your provisioning scripts set up in `~/.profile`, and in the project directory. Its arguments arrive unchanged; for pipes and the like, run `sh -c '...'`. It gets a terminal if sendit runs in one, so interactive programs work, e.g. `sendit run claude`, and its output can be piped otherwise. sendit exits with the command's exit code, or if the VM is stopped before the command ends, e.g. by `sendit stop` or Ctrl-C, with 128 plus the signal's number, as a shell would for a command killed by it. `sendit run` doesn't show the console then: it waits until the VM answers over SSH, runs the command, and shuts the VM down when it ends.

`sendit ssh` ignores `~/.ssh/config`: settings meant for trusted hosts, like `ForwardAgent yes` under `Host *`, would hand your SSH agent to the VM.

Inside the VM, the host is reachable as `host.sendit.internal`, whatever subnet macOS gives its VMs. Services on the host must listen on more than `127.0.0.1` for the VM to reach them.

In the console, `exit` (or Ctrl-D) shuts the VM down. Ctrl-] asks the guest to shut down; pressing it again forces the VM off. A shutdown that takes longer than 30 seconds is forced too.

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes these flags:

- `--cpus N`
- `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, read-only unless `:rw` is given, mounted at `/mnt/<name>` without `GUEST`. Repeatable. When a mount is dropped, the next boot removes its mount point if it is empty. Think twice before `:rw`: the VM can then change files that the Mac runs later, such as a crate's `build.rs` in `~/.cargo/registry` or a package in an npm cache.
- `--read-only`: shares the project directory read-only, e.g. to let an agent read code it shouldn't change. Builds that write into the project, such as `target/` or `node_modules/`, fail unless they are pointed elsewhere.
- `--expose-git`: lets the VM see and change `.git`, including its hooks and config, which git on the Mac runs.
- `--image NAME`: the base image to create the VM from. Later runs keep using it.

`run` asks before sharing a directory, the project or an extra mount, read-only or not, that contains your home directory, or that contains or sits inside sendit's own files (`~/.config/sendit`, `~/.cache/sendit`, `~/.sendit` or `$SENDIT_VM_DIR`), including what the symlinks among them point to and the symlinks on the way, e.g. a provisioning script linked from a dotfiles repository. Through those the VM could become root or run commands on the Mac.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, an `[images.<name>]` table overrides CPUs and memory for building that image and for the VMs made from it, and a `[projects."<path>"]` table overrides both for one project's VM. Command-line flags override all of them. Mounts from all layers are combined.

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

## Base images

A new VM is made from the `default` image unless `image` or `--image` picks another, and keeps using the image it was made from. `sendit provision NAME` builds the image called `NAME`; without a name, it builds the current project's image, and with `--all`, every image sendit knows of. Names are lowercase letters, digits and dashes.

An image file, `~/.config/sendit/images/NAME.sh`, adds custom provisioning to image `NAME`: it runs at the end of provisioning. Without `images/default.sh`, the `default` image is the plain base image. An image file can be a symlink, e.g. into a dotfiles repository; while it points to nothing, `sendit provision` refuses to build the image, and `run` and `status` warn and leave out the shares its scripts ask for. The image file is a bash script that calls the shared scripts in `~/.config/sendit/provision-scripts/` with `sendit-provision`:

```bash
sendit-provision rust.sh
sendit-provision helix.sh    # built with Rust, so it runs after it
sudo apt-get install -y postgresql-client
sendit-provision claude-code.sh
```

So it sets the order they run in, and can run commands of its own between them or call a script only `if` something holds. Shared scripts can call `sendit-provision` too. Each script runs at most once per build, the first time it's called, so several can call one they all need. The image file runs with `bash -e`: a command that fails, a script it calls included, stops the build. A script that fails also fails the build when the script that called it carries on, and calling it again returns its exit status again. To know which scripts an image needs before it builds it, sendit looks for `sendit-provision NAME` in the image file and, in turn, in the scripts it calls, outside of comments. So `NAME` must be a plain file name in `provision-scripts/`, written out rather than in a variable, and `sendit provision` refuses names it can't find and scripts that call each other in a cycle. `run` and `status` only warn about such mistakes, and about mount lines it can't read (see below), and leave out the shares the scripts ask for, so the VMs made from an image keep working while you edit its scripts.

Every script, the image file included, runs as the VM's user in a login shell of its own from the home directory, so it sees what the scripts before it added to `~/.profile`, e.g. to the `PATH`. After each `sendit-provision`, the script that called it reads `~/.profile` again, so its next commands see what the called script added. sudo is available only while provisioning runs. Trust the scripts, and whatever installers they download, as you would root in every VM made from the image: sendit takes the sudo rights away afterwards, but not a setuid binary or root service they may have left behind. `sendit status` and `sendit images` tell you when the image file or a script it calls has changed since an image was built, apart from the comments at their top; `sendit provision NAME --force` rebuilds it.

A script, the image file or one it calls, can ask for host directories to be shared with the VMs made from the image, in comment lines at its top, before its first command:

```sh
#!/bin/sh
# sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry
```

These take the same `HOST[:GUEST][:ro|rw]` as `--mount`, read-only unless `:rw` is given, and host paths must be absolute or start with `~`. They join the mounts of the config file and the command line, after the top-level ones and before the project's. A directory that doesn't exist on the Mac is left out with a notice instead of failing the run. `sendit provision` lists them, and `sendit status` shows them with the other mounts. A script's mounts apply to every image that calls it, even from an `if` that doesn't run. Read the mount lines of a script you copy: they give the VM access to files on the Mac, and with `:rw` it can change them.

[`provision-scripts/`](provision-scripts) has example scripts: Rust, the Helix editor, Claude Code, a `CLAUDE.md` that tells Claude Code about the VM (installed as its managed memory file, `/etc/claude-code/CLAUDE.md`, which a `~/.claude` shared from the Mac doesn't hide), and the Pi coding agent. [`images/example.sh`](images/example.sh) is an image file that calls most of them. Copy the scripts you want to `~/.config/sendit/provision-scripts/`, and an image file that calls them to `~/.config/sendit/images/`.

When `image` or `--image` picks a different image than the VM was made from, `sendit run` refuses to start it until `sendit reset` deletes it, so the next run starts from the new image. Rebuilding or deleting an image doesn't affect the VMs made from it.

### Git worktrees

`sendit run --worktree <branch>` checks out a branch in a worktree of the project's repository and boots the worktree's VM instead of the project's. The worktree goes next to the main checkout, into `<repo>.worktrees/<branch>`, with each `/` of the branch name replaced by `-`, e.g. `~/dev/repo.worktrees/feature-x` for `feature/x` in `~/dev/repo`; `--worktree-path` puts it elsewhere. A branch that doesn't exist yet starts at `HEAD`, or at `--from`; one that only a remote has tracks it, as with `git checkout`, and for one that several remotes have, `--from <remote>/<branch>` picks one. If the branch has a worktree already, `sendit run --worktree` boots that one's VM, so it also takes you back to the branch later. For a project in a subdirectory of the repository, as in a monorepo, it boots the VM of the same subdirectory of the worktree, e.g. `~/dev/repo.worktrees/feature-x/web` from `~/dev/repo/web`. sendit can't change your shell's directory, but the VM's console starts in the worktree.

A linked worktree (`git worktree add`) gets its own VM, like any other directory. If the main worktree has a VM and it is stopped, the worktree's VM starts as a copy of it instead of the base image: with everything installed, logged into and cached there, e.g. `~/.cargo/registry`. The copy is instant and takes no space until either VM writes. A copy of a running VM's disk could miss what the VM hasn't written out yet, so while the main worktree's VM runs, `sendit run` asks whether to start from the base image instead; the default is no, to stop the main worktree's VM first. `sendit run --fresh` starts the worktree's VM from the base image instead, of the main worktree's VM's image unless `--image` or the config file names another; for a worktree whose VM is a copy already, `sendit reset` it first. Both VMs keep the same SSH host keys and machine ID. A subdirectory of a worktree does the same with the VM of the same subdirectory of the main worktree. Without a `[projects."<path>"]` table of its own, a worktree uses its main worktree's, and a subdirectory of one that of the same subdirectory of the main worktree. `sendit status` shows which VM a worktree's VM is, or will be, a copy of.

## Limits

The VM is a boundary, but not one that makes it safe to run anything in it and then trust the project folder:

- Hiding `.git` relies on root in the VM staying out of reach. The read-only mount over it is made inside the VM, so anything that gets root there can remove it and change the history, hooks or config, which git on the Mac then runs. Ways to root include a Linux kernel exploit, something a provisioning script left behind, or a package installed with `sendit ssh --root`.
- Only the project's top-level `.git` is hidden, and only if it exists when the VM boots. The VM can plant a repository anywhere else in the project: a `.git` in a subfolder, a submodule's `.git` swapped for one of its own, or a top-level `.git` in a project that had none yet. Its config can make git run a command, e.g. through `core.fsmonitor`, as soon as git on the Mac looks inside: a `git status` in the project root is enough for a submodule, and so is a shell prompt or editor showing git status in a subfolder.
- More generally, anything the VM changes in the project folder may run on the Mac later: `build.rs`, `.cargo/config.toml`, Makefiles, `package.json` scripts, `.vscode/tasks.json`, test suites. Review what the VM changed before building or running it on the Mac.
- The VM's console output and `sendit ssh` sessions go straight to your terminal, including escape sequences. Depending on the terminal, these can set its title or, through OSC 52, the clipboard, so that a later paste into a shell on the Mac runs what the VM chose. Check your terminal's clipboard settings.
- The VM has network access. It can reach the internet, the local network, and every service on the Mac that listens on more than `127.0.0.1`, such as Remote Login or File Sharing.

## Where things live

Nothing is written into project directories.

- `~/.cache/sendit/`: Debian downloads, the base images (in `images/`), and SSH keys
- `~/.sendit/<project>_<uuid>/`: one directory per project VM
- `~/.config/sendit/`: configuration, image files (in `images/`) and the provisioning scripts they call (in `provision-scripts/`)

To keep the VMs somewhere else, e.g. on an external drive, set `SENDIT_VM_DIR` to an existing directory, such as `export SENDIT_VM_DIR=/Volumes/External/sendit`. sendit then uses it in place of `~/.sendit` and won't create it, so a drive that isn't connected is an error. It can't be the root of a volume, the home directory or a folder that contains it, and it can't contain or sit inside `~/.cache/sendit` or `~/.config/sendit`, since every start keeps it out of Time Machine backups. Folders in it that aren't VMs are left alone, by `sendit prune --all` too. On another volume, a new VM's disk is a copy of the base image instead of an instant clone, so each VM takes the base image's space on top of its own changes. Format the drive as APFS: HFS+ (Mac OS Extended) and exFAT have no sparse files, so there every VM takes its full disk size, 64 GiB by default. If the drive ignores ownership, which external drives often do, other users of the Mac can change the VMs' files and so run commands as root in them.

sendit excludes `~/.cache/sendit/` and the VM directory from Time Machine backups on every start. They hold only what sendit can make again, and a VM's disk changes whenever the VM runs, so each backup would copy all of it again. Keep anything inside a VM that you can't lose in the project directory, which lives on the Mac and is backed up as usual.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
