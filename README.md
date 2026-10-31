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

Commands act on the project in the current directory, on `-C DIR`, or on the project whose VM is named `-n NAME` (`--name`). `reset`, `prune` and `images --delete` ask first; `-y`/`--yes` skips that, and is needed without a terminal.

Each VM has a name of its own, so that you can reach it from any directory: `sendit -n my-app ssh`, or `sendit stop -n my-app`. A new VM is named after its project's folder, lowercased, with anything but letters and digits turned into dashes (`My App` becomes `my-app`), and `-2`, `-3`, … added if another VM has that name already; a git worktree's VM is named as described [below](#git-worktrees). `sendit list` and `sendit status` show the names, and VMs made by earlier versions of sendit get theirs the first time a command shows or looks up names, or makes or runs a VM. `sendit rename NEW` renames the VM, also while it runs: up to 64 lowercase letters, digits and dashes, not starting with a dash. The name goes with the VM, so after `sendit reset`, the next VM is named after the folder again. Found by name, a VM's project directory may be gone, e.g. for `sendit -n old-app reset`; only `run` needs it. When sendit suggests a command for a VM, e.g. after `sendit run -d` or in `sendit status`, it gives one that works where you are: outside the project directory, with `-n` and the VM's name.

A command given to `sendit run` or `sendit ssh` runs as the VM's user (or root, with `sendit ssh --root`) in a login shell in the project directory, so it gets the `PATH` your provisioning scripts set in `~/.profile`. Arguments arrive unchanged; use `sh -c '...'` for pipes and the like. Options before the command are sendit's, so a mistyped one is an error instead of the command; a command that starts with `-` goes after `--`. It gets a terminal when sendit has one, so `sendit run claude` works, and its output can be piped otherwise. sendit exits with the command's exit code, or with 128 plus the signal's number if the VM stops first, e.g. through `sendit stop` or Ctrl-C. With a command, `sendit run` skips the console, waits for SSH, and shuts the VM down when the command ends.

`sendit run -d` (`--detach`) runs the VM in the background, e.g. for a server in it that you reach from the Mac. sendit returns once the VM answers over SSH; then `sendit ssh` reaches it and `sendit stop` shuts it down, and closing the terminal doesn't. If you press Ctrl-C before that, or the VM doesn't answer within two minutes, sendit shuts it down again. Its console output goes to `run/console.log` in the VM's directory. A VM in the background keeps all the memory it has used until it stops; `sendit list` shows which ones are still running.

`sendit ssh` ignores `~/.ssh/config`, so settings like `ForwardAgent yes` under `Host *` don't hand your SSH agent to the VM.

Inside the VM, the host is `host.sendit.internal`. Host services must listen on more than `127.0.0.1` to be reachable, and be allowed with `network-allow` (see [Network](#network)). The VM takes the Mac's time zone on every boot.

In the console, `exit` or Ctrl-D shuts the VM down. Ctrl-] asks the guest to shut down, and a second press forces it off. Any shutdown that takes over 30 seconds, however it started, is forced too.

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes:

- `--cpus N` and `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, read-only unless `:rw`, at `/mnt/<name>` without `GUEST`. Repeatable. A dropped mount's empty mount point is removed on the next boot. Think twice before `:rw`: the VM can then change files the Mac runs later, such as a crate's `build.rs` in `~/.cargo/registry`.
- `--read-only`: shares the project read-only. Builds that write into it, such as `target/` or `node_modules/`, must be pointed elsewhere.
- `--expose-git`: lets the VM see and change `.git`, including the hooks and config git on the Mac runs.
- `--image NAME`: the base image to create the VM from. Later runs keep using it.
- `--network MODE` and `--network-allow DEST`: what the VM may connect to (see [Network](#network)). Repeatable for `--network-allow`.

`run` asks before sharing a directory, read-only or not (reading the root SSH key is enough), that contains your home directory, or that contains or sits inside sendit's own files (`~/.config/sendit`, `~/.cache/sendit`, `~/.sendit` or `$SENDIT_VM_DIR`), following symlinks, e.g. a provisioning script linked from a dotfiles repository: through those the VM could become root or run commands on the Mac. It checks when it sets up the shares, so a symlink made while a VM runs isn't caught until that VM's next run. sendit can't trace hard links, so link scripts and image files with symlinks.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, `[images.<name>]` overrides CPUs and memory for an image and its VMs, and `[projects."<path>"]` overrides both for one project. Flags override everything; mounts and `network-allow` entries from all layers are combined.

```toml
cpus = 4                  # default 2
memory = "8G"             # default 4G
disk-size = "128G"        # default 64G; the disk file is sparse and only grows
mounts = ["~/.cargo/registry:/home/dev/.cargo/registry"]  # read-only unless :rw
banner = false            # leave the "Send It" banner out of the login message
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
- `open`: everything, as with earlier versions of sendit: the internet, the local network, every service on the Mac that listens on more than `127.0.0.1`, other VMs, and whatever the Mac reaches through a VPN or Tailscale.

`network-allow` lets the VM connect to more, whatever `network` says, e.g. `["host:11434"]` for Ollama on the Mac. Its entries are `host[:PORT]` for the Mac (`host.sendit.internal`), `ADDR[/LEN][:PORT]` for IPv4, and `IPV6[/LEN]` or `[IPV6[/LEN]]:PORT` for IPv6; with a port, that port over TCP and UDP, and without one, every port. Entries from all layers of the config file are combined, and `--network-allow` adds more for one run, as `--network` picks the mode. `none` has no DNS either, so allow addresses, or the Mac as `host.sendit.internal`, which the VM resolves itself. `sendit status` shows both settings.

`internet` blocks the private and special-purpose address ranges: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `100.64.0.0/10` (Tailscale's), link-local and multicast addresses and the like, and for IPv6, unique local (`fc00::/7`), link-local and multicast addresses. It also blocks the networks of the Mac's own interfaces when the VM boots, which covers a local network or VPN with public addresses and the Mac's own IPv6 addresses, and all of these IPv4 addresses through NAT64 (in `64:ff9b::/96`), as well as `64:ff9b:1::/48`. A blocked TCP connection fails at once with "Connection refused", and sending anything else with "Operation not permitted". Blocked attempts show up in the kernel log, e.g. `sendit ssh --root journalctl -k -g sendit-network`.

The rules only filter connections that the VM, or a container in it, opens through its network card. Replies to connections from outside get through: `sendit ssh`, servers in the VM that the Mac reaches at the address `sendit status` shows, and devices on your tailnet reaching a VM that joined it with [`tailscale.sh`](provision-scripts/tailscale.sh). Traffic on other interfaces in the VM isn't filtered, such as Docker's bridges or Tailscale's `tailscale0`, where Tailscale's access controls decide what the VM reaches. Nor are connections into the VM: whatever reaches it, such as the Mac or another VM with `network = "open"`, can open one, and the VM answers over it.

The VM applies the rules itself, as root, on every boot, so a change takes effect when the VM next starts. Until they apply, and if they fail to, nothing gets out of the VM but DHCP; should the VM get no IPv4 route within 30 seconds, DNS and the Mac stay out of reach until it starts again, and the console says so. Base images built by earlier versions of sendit can't apply them and are outdated, along with their VMs (see [Base images](#base-images)).

## Base images

New VMs are made from the `default` image unless `image` or `--image` picks another, and keep using the image they were made from. If that setting later names a different image, `sendit run` refuses to start the VM until `sendit reset` deletes it. Rebuilding or deleting an image doesn't affect existing VMs.

A new version of sendit may change the base image in a way it relies on; the changelog says when. Images built before that are then outdated: `sendit status` and `sendit images` say so, and `sendit provision NAME`, or `--all`, builds them again. VMs made from them don't start until `sendit reset` deletes them, so that the next run makes them again; `sendit status` and `sendit list` tell which they are, and `sendit prune --outdated` deletes them all.

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

sendit finds the scripts an image needs by reading the `sendit-provision NAME` calls, skipping comments, quotes and here-documents (a `$(...)` in double quotes counts), so `NAME` must be the name of a file in `provision-scripts/` written out literally, and the only argument. Only files whose names end in `.sh` and don't start with a dot count as scripts there. The command must be the bare word `sendit-provision` too, not run through `eval`, `bash -c` or a full path, and that word anywhere outside quotes and comments counts as a call, so quote it in e.g. `command -v 'sendit-provision'`. `sendit provision` refuses names it can't find, scripts that call each other in a cycle, mount lines it can't read, and image files that are dangling symlinks (symlinks into e.g. a dotfiles repository are fine). `run` takes no shares from the scripts and doesn't fail on mistakes in them, so existing VMs keep working while you edit; it only stops while `provision-scripts/` has subfolders.

Each script runs as the VM's user in its own login shell from the home directory, as `bash FILE`, so its shebang line is ignored. `~/.profile` is read again after each `sendit-provision`, so later commands see what earlier scripts `export` in it, e.g. `PATH`. sudo is available only while provisioning. Trust the scripts, and whatever they download, as you would root in every VM made from the image: sendit takes sudo away afterwards, but not a setuid binary or root service they left behind.

`sendit status` and `sendit images` tell you when an image's scripts changed since it was built, apart from their leading comments other than mount lines; `sendit provision NAME --force` rebuilds it. `sendit status` also tells you when the project's VM was made from an earlier build of its image, and how to make it again from the latest one: `sendit reset`, then for a worktree's VM that would be a copy of a main VM from an earlier build too, `sendit run --fresh`; `sendit list` marks such VMs with "(earlier build)".

A script can ask for host directories to be shared with the image's VMs, in comment lines before its first command:

```bash
#!/bin/bash
# sendit-mount: ~/.cargo/registry:/home/dev/.cargo/registry
```

These take the same form as `--mount`, with host paths absolute or starting with `~`, and come after the top-level mounts and before the project's. A host directory that doesn't exist is left out with a notice. A script's mounts apply to every image that calls it, even from an `if` that doesn't run. They are recorded when the image is built, and each VM keeps those of the build it was made from: a changed mount line takes effect with the next build, for the VMs made after it, so rebuild the image, then `sendit reset` the VMs that should have it. A worktree's VM that is a copy of its main worktree's VM keeps that VM's, also when made again; `sendit run --fresh` makes it from the image instead. `sendit provision` lists them and `sendit status` shows them. Read the mount lines of a script you copy: they give the VM access to files on the Mac.

[`provision-scripts/`](provision-scripts) has example scripts:

| Script | What it does |
| --- | --- |
| [`rust.sh`](provision-scripts/rust.sh) | Installs Rust with rustup, with clippy and rustfmt |
| [`helix.sh`](provision-scripts/helix.sh) | Builds the Helix editor from source; calls `rust.sh` |
| [`claude-code.sh`](provision-scripts/claude-code.sh) | Installs Claude Code with its native installer |
| [`claude-md.sh`](provision-scripts/claude-md.sh) | Installs a `CLAUDE.md` that tells Claude Code about the VM, as `/etc/claude-code/CLAUDE.md`, so a `~/.claude` shared from the Mac doesn't hide it |
| [`pi.sh`](provision-scripts/pi.sh) | Installs Node.js and the Pi coding agent |
| [`tailscale.sh`](provision-scripts/tailscale.sh) | Joins each VM to your tailnet on boot as `sendit-<project>`, so your other devices can reach it; needs setup in the Tailscale admin console, described in the script |
| [`rex.sh`](provision-scripts/rex.sh) | Installs Rex and serves it over HTTPS through Tailscale Serve, so Rex on your other devices can reach the VM; calls `tailscale.sh` |

[`images/example.sh`](images/example.sh) calls most of them. Copy what you want to `~/.config/sendit/provision-scripts/` and `~/.config/sendit/images/`.

### Git worktrees

`sendit run --worktree <branch>` checks the branch out in a worktree and boots that worktree's VM. The worktree goes to `<repo>.worktrees/<branch>`, with `/` replaced by `-` (e.g. `~/dev/repo.worktrees/feature-x` for `feature/x`), or to `--worktree-path`. A new branch starts at `HEAD` or `--from`; a branch only a remote has is tracked as with `git checkout`, and `--from <remote>/<branch>` picks among several remotes. If the branch has a worktree already, its VM boots, so the same command takes you back later. For a project in a subdirectory of the repository, it boots the VM of the same subdirectory of the worktree. Your shell's directory doesn't change, but the console starts in the worktree.

Any linked worktree gets its own VM, named after the main worktree's VM (or folder, while that has none) and the worktree's folder: `my-app-feature-x` for the worktree in `my-app.worktrees/feature-x` of the project whose VM is `my-app`. If the main worktree has a stopped VM made from the same image, the worktree's VM starts as an instant copy of it, with everything installed, logged into and cached there, and the same SSH host keys and machine ID. A copy of a running VM could miss unwritten data, so while the main VM runs, `sendit run` asks whether to use the base image instead (default: no, stop the main VM first). `--fresh` starts from the base image, the main VM's unless `--image` or the config names another; for a worktree whose VM is a copy already, `sendit reset` it first. A worktree also uses its main worktree's `[projects."<path>"]` table unless it has its own. Subdirectories map to the same subdirectory of the main worktree for both. `sendit status` shows which VM a worktree's VM is, or will be, a copy of.

## Limits

The VM is a boundary, but not one that makes it safe to run anything in it and then trust the project folder:

- Hiding `.git` relies on root in the VM staying out of reach. The read-only mount is made inside the VM, so anything that gets root there can remove it and change the history, hooks or config that git on the Mac runs. Root could come from a kernel exploit, something a provisioning script left behind, or a package installed with `sendit ssh --root`.
- Only the top-level `.git` is hidden, and only if it exists at boot. The VM can plant a repository elsewhere: a `.git` in a subfolder, a submodule's `.git` swapped for its own, or a top-level one in a project that had none. Its config can run a command, e.g. through `core.fsmonitor`, as soon as git on the Mac looks inside: a `git status` in the project root is enough for a submodule, as is a shell prompt or editor showing git status in a subfolder.
- More generally, anything the VM changes may run on the Mac later: `build.rs`, `.cargo/config.toml`, Makefiles, `package.json` scripts, `.vscode/tasks.json`, test suites. Review the changes before building or running them on the Mac.
- Console output and `sendit ssh` sessions go straight to your terminal, escape sequences included. Depending on the terminal, these can set its title or, through OSC 52, the clipboard, so a later paste into a shell runs what the VM chose. Check your terminal's clipboard settings.
- The VM reaches the internet, so it can send what it reads anywhere, through DNS too. `network = "internet"` only keeps it from the local network, the Mac and other VMs, and only as far as addresses tell: it doesn't block a VPN on the Mac that routes public addresses, networks the Mac joins after the VM booted, other IPv6 networks of your router than the Mac's (e.g. a guest network), or what your router's public address reaches, such as its admin page or ports it forwards. A proxy on the Mac that answers DNS with fake addresses in `198.18.0.0/15`, which `internet` leaves open for it, can carry the VM's connections to names it resolves on the local network. With `network = "open"`, the VM reaches the local network and every service on the Mac that listens on more than `127.0.0.1`, such as Remote Login or File Sharing.
- Like the `.git` mask, the network rules hold only while root in the VM stays out of reach: root can lift them.

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
