# Send it

Send it (`sendit`) gives every project its own light and fast Debian VM on macOS, built on Virtualization.framework. It's meant for running things like coding agents against a project without giving them the rest of the machine.

A Debian base image is provisioned once. Each project's VM starts as an APFS clone of it, so creating one is instant and takes no space until the guest writes. The project directory is shared into the VM read-write, and its `.git` is hidden behind an empty read-only mount, so the VM can't see or change the history. The VM's user has no sudo rights; only the host can become root.

## Building

Requires macOS 27 on Apple silicon. `cargo build` and `cargo install --path .` link through `scripts/link-and-sign.sh`, which signs the binary with the virtualization entitlement.

## Usage

```sh
sendit provision          # download Debian and build the base image (once)
sendit run                # boot this project's VM and attach to its console
sendit ssh                # open another shell in the running VM
sendit ssh --root         # ... as root, e.g. to install packages
sendit stop               # shut the running VM down
sendit status             # show the VM and its effective settings
sendit reset              # delete the VM; the next run starts from a fresh copy
sendit list               # list all project VMs
sendit prune              # delete VMs whose project directory is gone
```

Commands act on the project in the current directory, or on the one given with `-C DIR`.

In the console, `exit` (or Ctrl-D) shuts the VM down. Ctrl-] asks the guest to shut down; pressing it again forces the VM off.

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes these flags:

- `--cpus N`
- `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, mounted at `/mnt/<name>` without `GUEST`. Repeatable.
- `--expose-git`: lets the VM see `.git`.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, and a `[projects."<path>"]` table overrides them for one project. Command-line flags override both. Mounts from all layers are combined.

```toml
cpus = 4                  # default 2
memory = "8G"             # default 4G
disk-size = "128G"        # default 64G; the disk file is sparse and only grows
mounts = ["~/.cargo/registry:/home/dev/.cargo/registry:ro"]

[provision]               # CPUs and memory for building the base image
memory = "8G"

[projects."~/Code/app"]
expose-git = true
```

Scripts in `~/.config/sendit/provision-scripts/*.sh` run in name order at the end of provisioning. They run as the VM's user, with sudo available only while they run. `sendit status` tells you when they have changed since the base image was built; `sendit provision --force` rebuilds it.

## Where things live

Nothing is written into project directories.

- `~/.cache/sendit/`: Debian downloads, the base images (in `images/`), and SSH keys
- `~/.sendit/<project>_<uuid>/`: one directory per project VM
- `~/.config/sendit/`: configuration and custom provisioning scripts
