#!/bin/bash
# Installs Rex (superlogical.com/rex) and runs its login server, so Rex for
# Mac and the rex CLI on your other devices can reach the VM over the
# tailnet, as dev: enter sendit-<project>.<tailnet>.ts.net in Add Remote
# Host…. Logging in on the console or with `sendit ssh` shows the address,
# and so does sendit-rex-address in the VM. Needs tailscale.sh, which it
# calls.
#
# The login server runs as root. It identifies each caller by their
# Tailscale login, through the VM's own tailscaled, and hands them a Rex
# server running as dev: the same one `rex` commands in the VM use, so
# whatever runs as dev there, the agent included, can see and type into
# those sessions. It listens on the VM's tailnet addresses only: HTTPS on
# port 443 through Tailscale Serve, and plain HTTP that Tailscale encrypts
# on port 7297. Rex for Mac refuses plain HTTP, so it needs Serve, and so
# HTTPS certificates turned on for the tailnet (admin console, DNS page);
# the rex CLI can use either. The VM keeps no Tailscale state, so the Serve
# configuration is set again at every boot. Each new VM gets a certificate
# from Let's Encrypt on its first HTTPS connection, which allows only 5 a
# week for the same name: reset one project's VM more often than that, and
# Rex for Mac can't reach it until the week is up.
#
# Any person on your tailnet may connect, as dev only; tagged devices,
# such as other sendit VMs, may not. root is never listed: it could unmount
# the mask over the project's .git. With other people on your tailnet,
# replace "tailscale:*" below with "tailscale:<your login>".
#
# Rex's package repository needs a token. On the Mac, save it as
# ~/.secrets/superlogical/token, then write the script that carries it into
# the build with
#   f=~/.config/sendit/provision-scripts/rex-secret.sh
#   (umask 077 && printf '%s\n' '#!/bin/bash' \
#       "printf '%s\\n' '$(cat ~/.secrets/superlogical/token)' | sudo install -D -m 600 /dev/stdin /etc/sendit-rex/package-token" \
#       > "$f") && chmod 600 "$f"
# and never commit or share it. apt keeps the token for updates, readable
# only by root, which this script checks. Updates come with a rebuilt image;
# until Rex has a stable release, it installs from the preview channel.
# After changing the token, run the command again: rex-secret.sh changes
# with it, so `sendit status` then says the image needs rebuilding.

sendit-provision tailscale.sh
sendit-provision rex-secret.sh

# One line holding the token alone: anything else, e.g. a Windows line
# ending, would only show as apt failing to log in.
if ! sudo awk '!/^[A-Za-z0-9._~+\/=-]+$/ { bad = 1 } END { exit bad || NR != 1 }' \
    /etc/sendit-rex/package-token; then
    echo 'rex.sh: /etc/sendit-rex/package-token should hold one line with the' \
        'package token; see rex-secret.sh' >&2
    exit 1
fi
token=$(sudo cat /etc/sendit-rex/package-token)
sudo rm -r /etc/sendit-rex
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
  --pam-service rex-login-server --extra-listen tailnet://:7297 \
  --extra-listen tailnet-serve:///run/rex/serve.sock
EOF

# Set again after every login (PartOf), since tailscaled forgets it. With
# HTTPS certificates off for the tailnet, `tailscale serve` waits for
# someone to turn them on instead of failing, so give up after a while;
# port 7297 works regardless.
sudo tee /etc/systemd/system/sendit-rex-serve.service > /dev/null <<'EOF'
[Unit]
Description=Serve Rex over HTTPS on the tailnet
Requires=sendit-tailscale.service
PartOf=sendit-tailscale.service
After=sendit-tailscale.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/tailscale serve --bg --yes --https=443 unix:/run/rex/serve.sock
TimeoutStartSec=30

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable rex-login-server.service sendit-rex-serve.service

# The address to add in Rex for Mac. While the VM is still booting, it may
# not be on the tailnet yet; wait for that up to the given number of
# seconds. The name is usually sendit-<project>, but Tailscale adds a
# number to it while an earlier device of that name is still around.
sudo tee /usr/local/bin/sendit-rex-address > /dev/null <<'EOF'
#!/bin/sh
set -u
wait=${1:-0}
while ! systemctl is-active --quiet sendit-rex-serve.service; do
    if [ "$wait" -le 0 ] || [ "$(systemctl is-system-running)" != starting ]; then
        echo 'not on the tailnet yet; see systemctl status sendit-tailscale sendit-rex-serve' >&2
        exit 1
    fi
    sleep 1
    wait=$((wait - 1))
done
name=$(tailscale status --json | jq -r '.Self.DNSName // empty')
if [ -z "$name" ]; then
    echo 'not on the tailnet' >&2
    exit 1
fi
echo "https://${name%.}"
EOF
sudo chmod 755 /usr/local/bin/sendit-rex-address

# Shown below the login message, on the console and in `sendit ssh`, but
# not for commands they run, nor in Rex itself.
sudo tee /etc/profile.d/sendit-rex.sh > /dev/null <<'EOF'
case $- in
*i*)
    if [ -z "${REX_SESSION-}" ]; then
        if sendit_rex=$(sendit-rex-address 10 2>&1); then
            printf '  Rex: %s (Add Remote Host… in Rex for Mac)\n\n' "$sendit_rex"
        else
            printf '  Rex: %s\n\n' "$sendit_rex"
        fi
        unset sendit_rex
    fi
    ;;
esac
EOF
