#!/bin/bash
# Installs Rex (superlogical.com/rex) and runs its login server, so Rex for
# Mac and the rex CLI on your other devices can reach the VM over the
# tailnet, as dev: http://sendit-<project>.<tailnet>.ts.net:7297, or just
# sendit-<project>.<tailnet>.ts.net in Add Remote Host…. Needs tailscale.sh,
# which it calls.
#
# UNTESTED: written from Rex's documentation before the Linux server could
# be installed. Check it against the release before relying on it.
#
# The login server runs as root. It identifies each caller by their
# Tailscale login, through the VM's own tailscaled, and hands them a Rex
# server running as dev: the same one `rex` commands in the VM use, so
# whatever runs as dev there, the agent included, can see and type into
# those sessions. It listens on the VM's tailnet addresses only (port 7297,
# plain HTTP that Tailscale encrypts), not through Tailscale Serve: the VM
# keeps no Tailscale state, so a Serve configuration would be gone after
# every boot.
#
# Any person on your tailnet may connect, as dev only; tagged devices,
# such as other sendit VMs, may not. root is never listed: it could unmount
# the mask over the project's .git. With other people on your tailnet,
# replace "tailscale:*" below with "tailscale:<your login>".
#
# Rex's package repository needs a token. On the Mac, save it as
# ~/.secrets/rex/token, then write the script that carries it into the
# build with
#   f=~/.config/sendit/provision-scripts/rex-secret.sh
#   (umask 077 && printf '%s\n' '#!/bin/bash' \
#       "printf '%s\\n' '$(cat ~/.secrets/rex/token)' | sudo install -D -m 600 /dev/stdin /etc/sendit-rex/package-token" \
#       > "$f") && chmod 600 "$f"
# and never commit or share it. apt keeps the token for updates, readable
# only by root, which this script checks. Updates come with a rebuilt image;
# until Rex has a stable release, it installs from the preview channel.

sendit-provision tailscale.sh
sendit-provision rex-secret.sh

token=$(sudo cat /etc/sendit-rex/package-token)
sudo rm -r /etc/sendit-rex
if [ -z "$token" ]; then
    echo 'rex.sh: rex-secret.sh wrote no token' >&2
    exit 1
fi
# Nobody else is in the VM while it is built, so the token may be an
# argument here.
curl -fsSL -u "rex:$token" https://pkgs.superlogical.com/rex/install.sh |
    sudo sh -s -- --token "$token"
rex --help > /dev/null

# install.sh decides where apt keeps the token; make sure only root can
# read it there.
readable=
while IFS= read -r -d '' file; do
    [ -z "$(sudo find "$file" -perm /044)" ] || readable="$readable $file"
done < <(sudo grep -rlZF -- "$token" /etc)
unset token
if [ -n "$readable" ]; then
    echo "rex.sh: the package token is readable by others in:$readable" >&2
    exit 1
fi

sudo tee /etc/rex/accounts.jsonc > /dev/null <<'EOF'
// Written by sendit's rex.sh. Only dev, never root.
{
  "version": 1,
  "accounts": {
    "dev": { "principals": ["tailscale:*"] },
  },
}
EOF

# After sendit-tailscale, since the tailnet addresses only exist once it
# has logged in. Restarting tailscaled (see tailscale.sh) restarts the
# login server too (Requires), after the new login; dev's server and its
# sessions keep running meanwhile.
sudo mkdir -p /etc/systemd/system/rex-login-server.service.d
sudo tee /etc/systemd/system/rex-login-server.service.d/sendit.conf > /dev/null <<'EOF'
[Unit]
Requires=tailscaled.service
After=tailscaled.service sendit-tailscale.service

[Service]
ExecStart=
ExecStart=/usr/bin/rex login-server --listen unix:///run/rex/login-server.sock \
  --accounts /etc/rex/accounts.jsonc --runtime-root /run/rex --log-format text \
  --pam-service rex-login-server --extra-listen tailnet://:7297
EOF
sudo systemctl daemon-reload
sudo systemctl enable rex-login-server.service
