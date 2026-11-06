Hi and thanks for checking out this project! First things first, this is completely vibe coded (except these two paragraphs). I still haven't made up my mind yet if I like or despise LLMs, but I believe it is a must for every developer to at least follow that space a little bit to stay up-to-date and, ideally, make their own experiences away from all the omnipotent hype that grasps every LLM community in existence (it seems).

I let Claude Code build this project based on my experiences with [vibe](https://github.com/lynaghk/vibe) and I truly want to thank the maintainer for putting it together. `vibe` is a very opinionated project (which I like!) and it served me well on the occasions when I would spin up an LLM (and believe me when I say these LLMs never got to see my production system). I just had a couple of ideas that wouldn't fit `vibe` very well, so I decided to gain some more experience with LLMs and create "Send it".

~SwishSwushPow

# Send it

Send it (`sendit`) gives every project its own light and fast Debian VM on macOS, built on Virtualization.framework, for running things like coding agents against a project without giving them the rest of the machine.

You provision a Debian 13 (trixie, arm64) base image once, or several with different tools. Each project's VM starts as an APFS clone of one: instant, and taking no space until the guest writes. The project directory is shared read-write, with `.git` hidden behind an empty read-only mount so the VM can't touch the history, as long as nothing in it becomes root (see [Limits](#limits)). The VM's user has no sudo rights; only the host can become root. The VM reaches the internet, but not your local network, the Mac or other VMs, unless you [allow them](#network).

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
sendit ssh                # open another shell in the running VM
sendit ssh cargo test     # ... or run a command there
sendit ssh --root         # ... as root, e.g. to install packages
sendit ssh-config         # let ssh and Rex reach each running VM as sendit-<name>
sendit stop               # shut the running VM down
sendit status             # show the VM and its effective settings
sendit reset              # delete the VM; the next run starts from a fresh copy
sendit rename web         # rename the VM to web
sendit -n web ssh         # any of these for the VM named web, from anywhere
sendit list               # list all project VMs and their names
sendit images             # list the base images and how many VMs use each
sendit images --delete go # delete the go image; its VMs keep working
sendit prune              # delete VMs whose project directory is gone
sendit prune --outdated   # ... and VMs made from an outdated base image
sendit prune --all        # delete every VM that isn't running
```

Commands act on the project in the current directory, on `-C DIR`, or on the project whose VM is named `-n NAME` (`--name`). `reset`, `prune`, `images --delete` and `ssh-config` ask first; `-y`/`--yes` skips that, and is needed without a terminal.

Each VM has a name, so you can reach it from any directory: `sendit -n my-app ssh`. A new VM is named after its project's folder, lowercased, with anything but letters and digits turned into dashes (`My App` becomes `my-app`), and `-2`, `-3`, … added if the name is taken; a git worktree's VM is named as described [below](#git-worktrees). `sendit rename NEW` renames the VM, also while it runs; a VM made before sendit 0.10 has no name until you give it one. The name goes with the VM, so after `sendit reset`, the next VM is named after the folder again. Found by name, a VM's project directory may be gone, e.g. for `sendit -n old-app reset`; only `run` needs it.

A command given to `sendit run` or `sendit ssh` runs as the VM's user (or root, with `sendit ssh --root`) in a login shell in the project directory, so it gets the `PATH` your provisioning scripts set in `~/.profile`. Arguments arrive unchanged; use `sh -c '...'` for pipes and the like, and `--` before a command that starts with `-`. It gets a terminal when sendit has one, so `sendit run claude` works, and its output can be piped otherwise. sendit exits with the command's exit code, or with 128 plus the signal's number if the VM stops first, e.g. through `sendit stop` or Ctrl-C. With a command, `sendit run` skips the console, waits for SSH, and shuts the VM down when the command ends.

`sendit run -d` (`--detach`) runs the VM in the background, e.g. for a server you reach from the Mac, and returns once the VM answers over SSH; closing the terminal doesn't stop it, `sendit stop` does. Its console output goes to `run/console.log` in the VM's directory. A VM in the background keeps all the memory it has used until it stops; `sendit list` shows which ones still run.

`sendit ssh` ignores `~/.ssh/config`, so settings like `ForwardAgent yes` under `Host *` don't hand your SSH agent to the VM.

`sendit ssh-config` lets ssh, and the programs that use it, reach each running VM as `sendit-<name>`: `ssh sendit-my-app`, `scp`, an editor's remote mode, or Rex as `ssh://sendit-my-app` (see [`rex.sh`](provision-scripts/rex.sh)). It writes a `Host sendit-*` block to `~/.cache/sendit/ssh/config` and asks to include it at the top of `~/.ssh/config`. For each connection, `sendit ssh-proxy` finds the VM by its name and checks its host key, as `sendit ssh` does. You log in as the VM's user, with agent and X11 forwarding off whatever your `Host *` settings say; others, such as `LocalForward`, still apply. The block names sendit's path, so run `sendit ssh-config` again after moving sendit.

Inside the VM, the host is `host.sendit.internal`. Host services must listen on more than `127.0.0.1` to be reachable, and be allowed with `network-allow` (see [Network](#network)). The VM takes the Mac's time zone on every boot.

In the console, `exit` or Ctrl-D shuts the VM down. Ctrl-] asks the guest to shut down, and a second press forces it off. Any shutdown that takes over 30 seconds, however it started, is forced too.

bash in the VM marks prompts and commands with OSC 133, so terminals such as Ghostty and Rex can jump between commands in the scrollback (Cmd+Up and Cmd+Down).

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes:

- `--cpus N` and `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, read-only unless `:rw`, at `/mnt/<name>` without `GUEST`. Repeatable. Think twice before `:rw`: the VM can then change files the Mac runs later, such as a crate's `build.rs` in `~/.cargo/registry`.
- `--read-only`: shares the project read-only. Builds that write into it, such as `target/` or `node_modules/`, must be pointed elsewhere.
- `--expose-git`: lets the VM see and change `.git`, including the hooks and config git on the Mac runs.
- `--image NAME`: the base image to create the VM from. Later runs keep using it.
- `--network MODE` and `--network-allow DEST`: what the VM may connect to (see [Network](#network)). Repeatable for `--network-allow`.

`run` asks before sharing a directory, even read-only, that contains your home directory, or that contains or sits inside sendit's own files (`~/.config/sendit`, `~/.cache/sendit`, `~/.sendit` or `$SENDIT_VM_DIR`), following symlinks, e.g. a provisioning script linked from a dotfiles repository: through those the VM could become root or run commands on the Mac. It also asks before sharing the project's git repository through another share, e.g. of a parent folder, or for a linked worktree, of the main worktree, as only the project's own share hides `.git`. sendit can't trace hard links, so link scripts and image files with symlinks.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, `[images.<name>]` overrides CPUs and memory for an image and its VMs, and `[projects."<path>"]` overrides both for one project. Flags override everything; mounts and `network-allow` entries from all layers are combined.

```toml
cpus = 4                  # default 2
memory = "8G"             # default 4G
disk-size = "128G"        # default 64G; the disk file is sparse and only grows
mounts = ["~/.cargo/registry:/home/dev/.cargo/registry"]  # read-only unless :rw
banner = false            # leave the "Send It" banner out of the login message
forward-changes = false   # don't forward the Mac's file changes into the VM; default true
network = "internet"      # the default; "none" blocks everything, "open" nothing
network-allow = ["host:11434"]  # let VMs reach port 11434 on the Mac, e.g. Ollama

image = "rust"            # the base image new VMs are made from; default "default"

[images.rust]             # CPUs and memory for building one image and its VMs
cpus = 6

[projects."~/Code/app"]
expose-git = true

[projects."~/Code/vendor"]
read-only = true          # share the project directory read-only
network = "none"          # no network at all
```

The VM swaps to compressed memory (zram) of up to half its `memory`, so a build that briefly needs more memory than the VM has slows down instead of being killed. An image file can turn that off with `sudo ln -sf /dev/null /etc/systemd/zram-generator.conf`, or retune it with a drop-in in `/etc/systemd/zram-generator.conf.d/`.

## Network

`network` says what a VM may connect to:

- `internet`, the default: the internet, but not the local network, the Mac or other VMs. DNS goes to the Mac.
- `none`: nothing.
- `open`: everything, as with earlier versions of sendit: the internet, the local network, every service on the Mac that listens on more than `127.0.0.1`, other VMs where macOS lets them, and whatever the Mac reaches through a VPN or Tailscale.

`network-allow` lets the VM connect to more, whatever `network` says, e.g. `["host:11434"]` for Ollama on the Mac. Entries are `host[:PORT]` for the Mac, `ADDR[/LEN][:PORT]` for IPv4, and `IPV6[/LEN]` or `[IPV6[/LEN]]:PORT` for IPv6; with a port, that port over TCP and UDP, and without one, every port. `none` has no DNS either, so allow addresses, or `host`: the VM resolves `host.sendit.internal` itself. `sendit status` shows both settings.

`internet` blocks private and special-purpose addresses, such as `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10` (Tailscale's), IPv6's `fc00::/7`, and link-local and multicast addresses; the networks of the Mac's own interfaces when the VM boots, which covers a local network or VPN with public addresses; and all of these through NAT64. A blocked TCP connection fails with "Connection refused", anything else with "Operation not permitted", and the kernel log shows it: `sendit ssh --root journalctl -k -g sendit-network`.

The rules filter only connections that the VM, or a container in it, opens through its network card. Connections into the VM get answers, e.g. `sendit ssh`, or the Mac reaching a server in it at the address `sendit status` shows. Traffic on other interfaces in the VM isn't filtered, such as Docker's bridges or Tailscale's `tailscale0`, where Tailscale's access controls decide. On macOS 27, other VMs can't reach it; on earlier versions, one with `network = "open"` may.

The VM applies the rules itself, as root, on every boot, so a change takes effect at its next start. Until they apply, and if they fail to, nothing but DHCP gets out, and the console says so.

## File changes

virtiofs gives programs in the VM no inotify events for files changed on the Mac, so watchers there (`inotifywait`, file watchers in dev servers, editors' language servers) would miss every edit made on the Mac. `sendit run` therefore watches the project directory with FSEvents and sends what changed to the VM, where a kernel module (`sendit_notify`, built into the base image with DKMS) raises the matching events: create, modify, close-write, delete and attrib. A rename on the Mac arrives as a delete of the old name and a create of the new one. A directory removed on the Mac, or moved out of the project (like Finder's Move to Trash), arrives as a delete for each file and directory in it, then for the directory itself, and watches on it end as they would on Linux. A directory moved into the project, or renamed in it, arrives as a create for itself and for everything in it. Changes usually arrive within a quarter of a second.

Events are the Mac's changes as sendit sees them, not one for each write:

- A path that changes again within a moment is reported once, as its final state, and an event can arrive twice.
- A file edited or chmod'ed within about 10 to 20 seconds of being created on the Mac can arrive as a create and a modify, since FSEvents keeps reporting it as created for that long.
- When the Mac is busy, FSEvents can drop events and only say where. sendit then compares that part of the project with what it last told the VM, and reports the differences as creates, modifies and deletes.
- If the VM stops reading for a while, e.g. while it is very busy or its service restarts, the changes wait until it reads again.
- Changes made before `sendit run` started aren't sent. Those made while the VM boots arrive once it is up.
- Watchers that don't look into a directory when they hear of it, such as `inotifywait -r`, can miss files created right after it, as they would without sendit.
- The events reach inotify and fanotify listeners that use file handles, not fanotify listeners that ask for file descriptors.

Only the project directory is forwarded, not extra `--mount`s, and `.git` only with `--expose-git`. Changes the VM makes itself already have their events, so the ones that come back from the Mac a moment later are dropped; a watcher in the VM sees its own writes once. A change on the Mac within a tenth of a second of the VM's own change to the same file can be taken for that one's echo, and dropped too.

`forward-changes = false` in the config file, or `sendit run --no-forward-changes`, turns it off; `sendit status` shows the setting as `changes`, also for a VM whose image doesn't have it. It needs an image built by this version: rebuild older ones with `sendit provision`, and make VMs from them again with `sendit reset`. Until then, the VM runs without it.

## Base images

New VMs are made from the `default` image unless `image` or `--image` picks another, and keep using the image they were made from. If that setting later names a different image, `sendit run` refuses to start the VM until `sendit reset` deletes it. Rebuilding or deleting an image doesn't affect existing VMs.

A new version of sendit may change the base image in a way it relies on; the changelog says when. Images built before that are then outdated: `sendit status` and `sendit images` say so, and `sendit provision NAME`, or `--all`, builds them again. VMs made from them don't start until `sendit reset` deletes them, so that the next run makes them again; `sendit status` and `sendit list` tell which they are, and `sendit prune --outdated` deletes them all.

`sendit provision NAME` builds image `NAME`; without a name, the current project's image, and with `--all`, every image sendit knows of; `--cpus` and `--memory` override what the build gets. Names are up to 64 lowercase letters, digits and dashes, not starting with a dash.

### Image files

`~/.config/sendit/images/NAME.sh` customizes image `NAME` and runs at the end of provisioning; without `default.sh`, the `default` image is the plain base image. It's a bash script that calls shared scripts from `~/.config/sendit/provision-scripts/` with `sendit-provision`:

```bash
sendit-provision rust.sh
sendit-provision helix.sh
sudo apt-get install -y postgresql-client
sendit-provision tailscale.sh
sendit-provision rex-tailscale.sh    # calls tailscale.sh too, which runs only once
```

The image file sets the order, and can run its own commands in between or call a script only `if` something holds. Scripts can call `sendit-provision` too; each runs at most once per build, so several can share one they all need. The image file and the scripts run with `bash -e -o pipefail`, so a failing command stops the script it's in, and a failed script fails the build, even if the script that called it carries on.

sendit finds the scripts an image needs by reading the `sendit-provision NAME` calls outside comments, quotes and here-documents, so write both out literally, with `NAME` a `.sh` file in `provision-scripts/`, not through a variable, `eval` or `bash -c`. `sendit provision` refuses what it can't follow, such as unknown names or scripts that call each other in a cycle; `run` doesn't fail on such mistakes, so existing VMs keep working while you edit. Image files can be symlinks, e.g. into a dotfiles repository.

Each script runs as the VM's user in its own login shell from the home directory, as `bash FILE`, so its shebang line is ignored. `~/.profile` is read again after each `sendit-provision`, so later commands see what earlier scripts `export` in it, e.g. `PATH`. sudo is available only while provisioning. Trust the scripts, and whatever they download, as you would root in every VM made from the image: sendit takes sudo away afterwards, but not a setuid binary or root service they left behind.

`sendit status` and `sendit images` tell you when an image's scripts changed since it was built, apart from their leading comments other than mount lines; `sendit provision NAME --force` rebuilds it. `sendit status` and `sendit list` also tell you when a VM was made from an earlier build of its image, and `status` how to make it again.

A script can ask for host directories to be shared with the image's VMs, in comment lines before its first command:

```bash
#!/bin/bash
# sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry
```

These take the same form as `--mount`, with host paths absolute or starting with `~`, and come after the top-level mounts and before the project's. A host directory that doesn't exist is left out with a notice. A script's mounts apply to every image that calls it, even from an `if` that doesn't run. Each VM keeps those of the build it was made from, so a changed mount line takes a rebuild of the image and a `sendit reset` of the VMs that should get it. `sendit provision` lists them and `sendit status` shows them. Read the mount lines of a script you copy: they give the VM access to files on the Mac.

[`provision-scripts/`](provision-scripts) has example scripts:

| Script | What it does |
| --- | --- |
| [`rust.sh`](provision-scripts/rust.sh) | Installs Rust with rustup, with clippy and rustfmt |
| [`helix.sh`](provision-scripts/helix.sh) | Installs the Helix editor from its release build, grammars included |
| [`claude-code.sh`](provision-scripts/claude-code.sh) | Installs Claude Code with its native installer |
| [`claude-md.sh`](provision-scripts/claude-md.sh) | Installs a `CLAUDE.md` that tells Claude Code about the VM, as `/etc/claude-code/CLAUDE.md`, so a `~/.claude` shared from the Mac doesn't hide it |
| [`pi.sh`](provision-scripts/pi.sh) | Installs Node.js and the Pi coding agent |
| [`tailscale.sh`](provision-scripts/tailscale.sh) | Joins each VM to your tailnet on boot as `sendit-<project>`, so your other devices can reach it; needs setup in the Tailscale admin console, described in the script |
| [`rex.sh`](provision-scripts/rex.sh) | Installs Rex and runs a Rex server as the VM's user from boot, which Rex for Mac reaches over SSH with [`sendit ssh-config`](#usage) |
| [`rex-tailscale.sh`](provision-scripts/rex-tailscale.sh) | Rex as `rex.sh` sets it up, and also served over HTTPS through Tailscale Serve, so Rex on your other devices, e.g. Rex for iOS, can reach the VM; calls `rex.sh` and `tailscale.sh` |

[`images/example.sh`](images/example.sh) calls some of them. Copy what you want to `~/.config/sendit/provision-scripts/` and `~/.config/sendit/images/`.

### Git worktrees

`sendit run --worktree <branch>` checks the branch out in a worktree and boots that worktree's VM. The worktree goes to `<repo>.worktrees/<branch>`, with `/` replaced by `-` (e.g. `~/dev/repo.worktrees/feature-x` for `feature/x`), or to `--worktree-path`. A new branch starts at `HEAD` or `--from`; a branch only a remote has is tracked as with `git checkout`, and `--from <remote>/<branch>` picks among several remotes. If the branch has a worktree already, its VM boots, so the same command takes you back later. For a project in a subdirectory of the repository, it boots the VM of the same subdirectory of the worktree. Your shell's directory doesn't change, but the console starts in the worktree.

Any linked worktree gets its own VM, named after the main worktree's VM (or folder, while that has none) and the worktree's folder: `my-app-feature-x` for the worktree in `my-app.worktrees/feature-x` of the project whose VM is `my-app`. If the main worktree has a stopped VM made from the same image, the worktree's VM starts as an instant copy of it, with everything installed, logged into and cached there, and the same SSH host keys and machine ID. A copy of a running VM could miss unwritten data, so while the main VM runs, `sendit run` asks whether to use the base image instead (default: no, stop the main VM first). `--fresh` starts from the base image, the main VM's unless `--image` or the config names another; for a worktree whose VM is a copy already, `sendit reset` it first. A worktree also uses its main worktree's `[projects."<path>"]` table unless it has its own. Subdirectories map to the same subdirectory of the main worktree for both. `sendit status` shows which VM a worktree's VM is, or will be, a copy of.

## Limits

The VM is a boundary, but not one that makes it safe to run anything in it and then trust the project folder:

- Hiding `.git` relies on root in the VM staying out of reach. The read-only mount is made inside the VM, so anything that gets root there can remove it and change the history, hooks or config that git on the Mac runs. Root could come from a kernel exploit, something a provisioning script left behind, or a package installed with `sendit ssh --root`.
- Only the top-level `.git` is hidden, and only if it exists at boot. The VM can plant a repository elsewhere: a `.git` in a subfolder, a submodule's `.git` swapped for its own, or a top-level one in a project that had none. Its config can run a command, e.g. through `core.fsmonitor`, as soon as git on the Mac looks inside: a `git status` in the project root is enough for a submodule, as is a shell prompt or editor showing git status in a subfolder.
- More generally, anything the VM changes may run on the Mac later: `build.rs`, `.cargo/config.toml`, Makefiles, `package.json` scripts, `.vscode/tasks.json`, test suites. Review the changes before building or running them on the Mac.
- Console output and `sendit ssh` sessions go straight to your terminal, escape sequences included. Depending on the terminal, these can set its title or, through OSC 52, the clipboard, so a later paste into a shell runs what the VM chose. Check your terminal's clipboard settings.
- The VM reaches the internet, so it can send what it reads anywhere, through DNS too. `network = "internet"` keeps it from the local network, the Mac and other VMs only as far as addresses tell: not from a VPN on the Mac that routes public addresses, networks the Mac joins after the VM booted, your router's other IPv6 networks (e.g. a guest network), what your router's public address reaches (its admin page, forwarded ports), or local names that a proxy on the Mac answering DNS with fake addresses in `198.18.0.0/15` resolves. With `network = "open"`, the VM reaches the local network and every service on the Mac that listens on more than `127.0.0.1`, such as Remote Login or File Sharing.
- Like the `.git` mask, the network rules hold only while root in the VM stays out of reach: root can lift them.
- If you harden a VM by setting `kernel.modules_disabled=1`, do it after `sendit-changes.service` has started, which loads the `sendit_notify` module at boot; otherwise file changes from the Mac no longer arrive. The module isn't signed with a key the kernel trusts, so `lockdown=` on the kernel command line keeps it from loading, too.

## Where things live

Nothing is written into project directories.

- `~/.cache/sendit/`: Debian downloads, the base images (in `images/`), and SSH keys and the Host block of `sendit ssh-config` (in `ssh/`)
- `~/.sendit/<project>_<uuid>/`: one directory per project VM, with its lock, `.<project>_<uuid>.lock`, next to it
- `~/.config/sendit/`: configuration, image files (in `images/`) and the provisioning scripts they call (in `provision-scripts/`)

sendit excludes `~/.cache/sendit/` and the VM folder (`~/.sendit` or `$SENDIT_VM_DIR`) from Time Machine backups: they hold only what sendit can make again, and a VM's disk changes on every run. Keep anything you can't lose in the project directory. It also makes both accessible only to you, since base images, VM disks and the files of a build hold whatever the provisioning scripts put there, secrets included.

To keep the VMs elsewhere, e.g. on an external drive, set `SENDIT_VM_DIR` to a folder only for sendit, which it uses in place of `~/.sendit`: anything else in it isn't backed up either, and sendit warns about it. sendit creates the folder, but not the folders it is in, so a drive that isn't connected is an error, not VMs made on your Mac. Things to know about other drives:

- A VM's disk is a copy of the base image instead of an instant clone, so each VM takes the base image's space on top of its own changes.
- Format the drive as APFS: HFS+ and exFAT have no sparse files, so every VM takes its full disk size, 64 GiB by default.
- If the drive ignores ownership, as external drives often do, other users of the Mac can change the VMs' files and so run commands as root in them.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
