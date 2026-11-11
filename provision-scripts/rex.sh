#!/bin/bash
# Installs Rex (superlogical.com/rex) and runs a Rex server as dev, which
# Rex for Mac and the rex CLI reach over SSH: run `sendit ssh-config` once
# on the Mac, then add the host ssh://sendit-<name> in Rex for Mac (Add
# Remote Host…), or use `rex -S ssh://sendit-<name>`; `sendit list` shows
# the names. To reach the VM from your other devices over your tailnet too,
# e.g. from Rex for iOS, call rex-tailscale.sh instead, which calls this
# one.
#
# The server is the one `rex` commands in the VM use, so whatever runs as
# dev there, the agent included, can see and type into its sessions. It
# starts at boot and runs while nobody is logged in (lingering), so its
# sessions outlive the SSH connection of Rex for Mac, e.g. while the Mac
# sleeps. systemd also holds its socket (rex.socket), so that a server
# stopped with `rex server stop` comes back on the next connection. The
# server saves its sessions when the VM shuts down and brings them back at
# the next boot, as new shells in the directories they were in.
#
# Rex installs from its public package repository, on the preview channel:
# without --channel, install.sh would fall back to snapshot builds until Rex
# has a stable release. Updates come with a rebuilt image.

curl -fsSL https://pkgs.superlogical.com/rex/install.sh |
    sudo sh -s -- --channel preview
# Debian has an unrelated package named rex, (R)?ex, which has no version
# command, so this also makes sure apt installed Rex.
rex version > /dev/null

# What `systemctl --user enable rex.socket rex.service` does, which needs
# dev's user manager, and that doesn't run while the image is built. Both:
# with the socket alone, the server would start only on the first
# connection, and Rex's login server (rex-tailscale.sh) fails that one.
mkdir -p ~/.config/systemd/user/sockets.target.wants ~/.config/systemd/user/default.target.wants
ln -sf /usr/lib/systemd/user/rex.socket ~/.config/systemd/user/sockets.target.wants/rex.socket
ln -sf /usr/lib/systemd/user/rex.service ~/.config/systemd/user/default.target.wants/rex.service
# What `loginctl enable-linger` does from the next boot on. That command
# would also start dev's user manager now, and with it the server, which
# would leave the session it starts with in the image, for every VM.
sudo install -d /var/lib/systemd/linger
sudo touch "/var/lib/systemd/linger/$(id -un)"
