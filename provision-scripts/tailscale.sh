#!/bin/bash
# Joins every VM made from the image to your tailnet on boot, as an
# ephemeral device named sendit-<project> (sendit-feature-x for a worktree
# in repo.worktrees/feature-x) with the tag tag:sendit, so your other
# devices can reach it, e.g. `ssh dev@sendit-<project>` with Tailscale SSH.
# `sendit run` and `sendit ssh` keep going through the VM's own network:
# booting doesn't wait for the tailnet, and works without it.
#
# The VM keeps no Tailscale state on disk (tailscaled --state=mem:), so
# every boot registers a new device, which tailscaled logs out and so
# removes on shutdown, or the tailnet removes a while after a forced one.
# A worktree's VM, a copy of its main VM, gets its own device too. The
# name stays; the Tailscale IP may change from boot to boot. Should the
# tailnet remove a running VM's device, e.g. after the Mac slept for over
# half an hour or you removed it in the admin console, the VM logs in again
# within a few minutes.
#
# It authenticates with an OAuth client secret, which provisioning puts in
# a file only root can read: the agent, running as dev, can't read it and
# add devices of its own. The OAuth client makes a one-off key at every
# boot, so nothing expires.
#
# Setup, in the Tailscale admin console:
# 1. Access controls: let tag:sendit exist, let your devices reach it and
#    log in as dev (never root, which could unmount the hidden .git). Keep
#    tag:sendit out of every "src": a tagged device can only start
#    connections that the policy grants it, so the agent can't reach the
#    rest of your tailnet. That means replacing the default policy's
#    allow-all grant, whose "*" source includes tag:sendit, with the
#    "autogroup:member" one below, which lets your own devices reach each
#    other as before.
#      "tagOwners": { "tag:sendit": ["autogroup:admin"] },
#      "grants": [
#        { "src": ["autogroup:member"], "dst": ["*"], "ip": ["*"] },
#      ],
#      "ssh": [
#        { "action": "accept", "src": ["autogroup:member"],
#          "dst": ["tag:sendit"], "users": ["dev"] },
#      ],
# 2. Settings > OAuth clients: generate a client with write access to
#    Keys > Auth Keys and the tag tag:sendit. Copy its secret
#    (tskey-client-...); it is shown only once.
# 3. On the Mac, save it as ~/.secrets/tailscale/token. Provisioning runs
#    in the VM, which can't read files on the Mac, so the secret travels in
#    a provisioning script of its own: write that from the token with
#      f=~/.config/sendit/provision-scripts/tailscale-secret.sh
#      (umask 077 && printf '%s\n' '#!/bin/bash' \
#          "printf '%s\\n' '$(cat ~/.secrets/tailscale/token)' | sudo install -D -m 600 /dev/stdin /etc/sendit-tailscale/oauth-secret" \
#          > "$f") && chmod 600 "$f"
#    and never commit or share it. A copy of it stays in
#    ~/.cache/sendit/provision/ on the Mac, which sendit keeps private to
#    you, and in the image and its VMs, readable only by root there. To
#    revoke it, delete the OAuth client in the admin console.
#
# After changing the token, run the command again: tailscale-secret.sh
# changes with it, so `sendit status` then says the image needs rebuilding.
# To keep one VM off the tailnet, run
# `sendit ssh --root systemctl disable --now sendit-tailscale` for it,
# which logs it out right away.

sendit-provision tailscale-secret.sh

# One line holding the secret alone: anything else, e.g. a Windows line
# ending, would only show at boot, as a VM that never joins.
if ! sudo awk '!/^tskey-client-[A-Za-z0-9-]+$/ { bad = 1 } END { exit bad || NR != 1 }' \
    /etc/sendit-tailscale/oauth-secret; then
    echo 'tailscale.sh: /etc/sendit-tailscale/oauth-secret should hold one line with an' \
        'OAuth client secret (tskey-client-...); see tailscale-secret.sh' >&2
    exit 1
fi
# tailscale up reads this file instead of taking the secret as an argument,
# where dev could read it in ps. Ephemeral is the default, but say so;
# without preauthorized, the key the client makes would need approval in
# the admin console if device approval is on.
sudo sh -c 'umask 077 && printf "%s?ephemeral=true&preauthorized=true\n" \
    "$(cat /etc/sendit-tailscale/oauth-secret)" > /etc/sendit-tailscale/authkey'
sudo rm /etc/sendit-tailscale/oauth-secret

sudo tee /etc/default/sendit-tailscale > /dev/null <<'EOF'
# Read by sendit-tailscale.service; see tailscale.sh.
TAG=tag:sendit
HOSTNAME_PREFIX=sendit-
EOF

# tailscaled keeps its state in memory only, which also makes the device
# ephemeral. /var/lib/tailscale keeps caches, e.g. Tailscale SSH's host
# keys. It logs out on shutdown, so give it time for that, but stop well
# before sendit forces the VM off after 30 seconds, and before
# systemd-networkd, which drops the VM's address when it stops. Should it
# crash over and over, keep restarting it, but not within a split second:
# hitting the start limit would leave the VM off the tailnet until reboot.
sudo mkdir -p /etc/systemd/system/tailscaled.service.d
sudo tee /etc/systemd/system/tailscaled.service.d/sendit.conf > /dev/null <<'EOF'
[Unit]
After=systemd-networkd.service
StartLimitIntervalSec=0

[Service]
ExecStart=
ExecStart=/usr/sbin/tailscaled --state=mem: --statedir=/var/lib/tailscale --socket=/run/tailscale/tailscaled.sock --port=${PORT} $FLAGS
RestartSec=5
TimeoutStopSec=10
EOF

# Updates come with a rebuilt image, as for every other package.
curl -fsSL https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg |
    sudo tee /usr/share/keyrings/tailscale-archive-keyring.gpg > /dev/null
curl -fsSL https://pkgs.tailscale.com/stable/debian/trixie.tailscale-keyring.list |
    sudo tee /etc/apt/sources.list.d/tailscale.list > /dev/null
sudo apt-get update
sudo apt-get -y install --no-install-recommends tailscale

# The project's directory names the device; sendit's mount manifest says
# where it is. Tailscale SSH serves only connections over the tailnet, so
# `sendit ssh` still reaches OpenSSH. The tailnet's DNS and routes are left
# out: the VM needs nothing from the tailnet, and they could get in the way
# of its own network. Without state, tailscaled would take the tailnet's
# default for automatic updates at every boot, and could run apt in the
# background; updates come with a rebuilt image instead.
sudo tee /usr/local/sbin/sendit-tailscale > /dev/null <<'EOF'
#!/bin/sh
set -eu
dir=$(sed -n 's/^workdir //p' /run/sendit/meta/mounts 2> /dev/null | head -n 1)
name=$(basename "${dir:-vm}" | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-' |
    sed 's/^-*//; s/-*$//')
hostname=$(printf '%s%s' "$HOSTNAME_PREFIX" "${name:-vm}" | cut -c 1-63 | sed 's/-*$//')
tailscale up --auth-key=file:/etc/sendit-tailscale/authkey \
    --advertise-tags="$TAG" --hostname="$hostname" --ssh \
    --accept-dns=false --accept-routes=false --timeout=60s
exec tailscale set --auto-update=false
EOF
sudo chmod 755 /usr/local/sbin/sendit-tailscale

# The console and SSH don't wait for this, so they come up without the
# tailnet; only the end of booting (systemctl is-system-running) does.
# Without a network, it tries again, after 30 seconds at first and up to
# 10 minutes apart, so that a deleted OAuth client isn't asked for keys
# forever; `tailscale up` waits without a limit while it gets its key, so
# taking too long counts as failing. A restart of tailscaled starts it
# with an empty state, so log in again then. Stopping it logs out, which
# removes the device, and has to fit sendit's 30 seconds for shutting
# down along with tailscaled's own stop.
sudo tee /etc/systemd/system/sendit-tailscale.service > /dev/null <<'EOF'
[Unit]
Description=Join the tailnet
Requires=tailscaled.service
PartOf=tailscaled.service
Wants=network-online.target
After=network-online.target tailscaled.service sendit-mounts.service
StartLimitIntervalSec=0

[Service]
Type=oneshot
RemainAfterExit=yes
EnvironmentFile=/etc/default/sendit-tailscale
ExecStart=/usr/local/sbin/sendit-tailscale
TimeoutStartSec=3min
ExecStop=-/usr/bin/tailscale logout
TimeoutStopSec=5
Restart=on-failure
RestartSec=30
RestartSteps=5
RestartMaxDelaySec=10min

[Install]
WantedBy=multi-user.target
EOF

# The tailnet removes an ephemeral device that has been quiet for a while,
# e.g. while the Mac sleeps and the VM with it. tailscaled (1.104) then
# keeps asking the coordination server for its network map, which answers
# 404, and logs that; later versions wait for a login (NeedsLogin)
# instead. Restarting it logs in again, with sendit-tailscale (PartOf).
# Only the running tailscaled's log counts, so that a 404 from before the
# restart doesn't restart it again. The check leaves a VM alone whose
# sendit-tailscale is disabled, failing or still logging in. It is frozen
# too while the Mac sleeps, so it never keeps the Mac awake. A check that
# hangs would hold up all later ones, so it has a time limit.
sudo tee /usr/local/sbin/sendit-tailscale-check > /dev/null <<'EOF'
#!/bin/sh
set -eu
systemctl is-active --quiet sendit-tailscale.service || exit 0
state=$(tailscale status --json 2> /dev/null | jq -r .BackendState) || exit 0
removed() {
    id=$(systemctl show -P InvocationID tailscaled.service)
    [ -n "$id" ] && journalctl -q -o cat --since -3min _SYSTEMD_INVOCATION_ID="$id" |
        grep -q 'initial fetch failed 404'
}
if [ "$state" = NeedsLogin ] || removed; then
    echo 'The tailnet dropped this device; logging in again.'
    exec systemctl restart tailscaled.service
fi
EOF
sudo chmod 755 /usr/local/sbin/sendit-tailscale-check
sudo tee /etc/systemd/system/sendit-tailscale-check.service > /dev/null <<'EOF'
[Unit]
Description=Log in to the tailnet again if it dropped the VM

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/sendit-tailscale-check
TimeoutStartSec=2min
EOF
sudo tee /etc/systemd/system/sendit-tailscale-check.timer > /dev/null <<'EOF'
[Unit]
Description=Check every minute that the VM is still on the tailnet

[Timer]
OnBootSec=1min
OnUnitActiveSec=1min

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable tailscaled.service sendit-tailscale.service sendit-tailscale-check.timer

# The package started tailscaled; leave nothing of that in the image.
sudo systemctl stop tailscaled.service
sudo find /var/lib/tailscale /var/cache/tailscale -mindepth 1 -delete 2> /dev/null || true
