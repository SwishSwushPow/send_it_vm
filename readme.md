Hi and thanks for checking out this project! First things first, this is completely vibe coded (except these two paragraphs). I still haven't made up my mind yet if I like or despise LLMs, but I believe it is a must for every developer to at least follow that space a little bit to stay up-to-date and, ideally, make their own experiences away from all the omnipotent hype that grasps every LLM community in existence (it seems).

I let Claude Code build this project based on my experiences with [vibe](https://github.com/lynaghk/vibe) and I truly want to thank the maintainer for putting it together. `vibe` is a very opinionated project (which I like!) and it served me well on the occasions when I would spin up an LLM (and believe me when I say these LLMs never got to see my production system). I just had a couple of ideas that wouldn't fit `vibe` very well, so I decided to gain some more experience with LLMs and create "Send it".

~SwishSwushPow

# Send it

Send it (`sendit`) gives every project its own light and fast Debian VM on macOS, built on Virtualization.framework. It's meant for running things like coding agents against a project without giving them the rest of the machine.

A Debian base image is provisioned once, or several with different tools installed. Each project's VM starts as an APFS clone of one, so creating one is instant and takes no space until the guest writes. The project directory is shared into the VM read-write, and its `.git` is hidden behind an empty read-only mount, so the VM can't see or change the history. The VM's user has no sudo rights; only the host can become root.

## Building

Requires macOS 27 on Apple silicon. Install with `cargo install send_it_vm`, or `cargo install --path .` from a clone.

Virtualization.framework only works for binaries signed with the virtualization entitlement. `cargo build` and `cargo install --path .` link through `scripts/link-and-sign.sh`, which signs the binary. A binary installed from crates.io is built without it, so on its first run `sendit` signs itself (ad hoc) and starts again.

## Usage

```sh
sendit provision          # download Debian and build this project's base image (once)
sendit run                # boot this project's VM and attach to its console
sendit ssh                # open another shell in the running VM
sendit ssh --root         # ... as root, e.g. to install packages
sendit stop               # shut the running VM down
sendit status             # show the VM and its effective settings
sendit reset              # delete the VM; the next run starts from a fresh copy
sendit list               # list all project VMs
sendit images             # list the base images and how many VMs use each
sendit prune              # delete VMs whose project directory is gone
```

Commands act on the project in the current directory, or on the one given with `-C DIR`.

In the console, `exit` (or Ctrl-D) shuts the VM down. Ctrl-] asks the guest to shut down; pressing it again forces the VM off.

The project is mounted at `/home/dev/<name>`, where login shells start. `run` also takes these flags:

- `--cpus N`
- `--memory 8G`
- `--mount HOST[:GUEST][:ro|rw]`: shares another directory, mounted at `/mnt/<name>` without `GUEST`. Repeatable.
- `--expose-git`: lets the VM see `.git`.
- `--image NAME`: the base image to create the VM from. Later runs keep using it.

## Configuration

`~/.config/sendit/config.toml`. Top-level keys apply to all projects, and a `[projects."<path>"]` table overrides them for one project. Command-line flags override both. Mounts from all layers are combined.

```toml
cpus = 4                  # default 2
memory = "8G"             # default 4G
disk-size = "128G"        # default 64G; the disk file is sparse and only grows
mounts = ["~/.cargo/registry:/home/dev/.cargo/registry:ro"]

image = "rust"            # the base image new VMs are made from; default "default"

[provision]               # CPUs and memory for building base images
memory = "8G"

[images.rust]             # ... and for building one of them
memory = "12G"

[projects."~/Code/app"]
expose-git = true
```

## Base images

A new VM is made from the `default` image unless `image` or `--image` picks another, and keeps using the image it was made from. `sendit provision NAME` builds the image called `NAME`; without a name, it builds the current project's image, and with `--all`, every image sendit knows of. Names are lowercase letters, digits and dashes.

Scripts in `~/.config/sendit/provision-scripts/*.sh` run for every image at the end of provisioning, and scripts in `provision-scripts/NAME/*.sh` only for image `NAME`. They run together in file name order; an image's own script replaces a shared one with the same file name. They run as the VM's user, with sudo available only while they run. `sendit status` and `sendit images` tell you when they have changed since an image was built; `sendit provision NAME --force` rebuilds it.

[`provision-scripts/`](provision-scripts) has example scripts: Rust, the Helix editor, Claude Code and the Pi coding agent. Copy the ones you want with a number in front to set the order, e.g. `00-rust.sh` before `01-helix.sh`, which builds Helix with Rust.

When `image` or `--image` picks a different image than the VM was made from, `sendit run` refuses to start it until `sendit reset` deletes it, so the next run starts from the new image. Rebuilding or deleting an image doesn't affect the VMs made from it.

## Where things live

Nothing is written into project directories.

- `~/.cache/sendit/`: Debian downloads, the base images (in `images/`), and SSH keys
- `~/.sendit/<project>_<uuid>/`: one directory per project VM
- `~/.config/sendit/`: configuration and custom provisioning scripts

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
