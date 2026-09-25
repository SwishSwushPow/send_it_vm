#!/bin/bash
# Provisions the sendit base image. Runs once, as root, from cloud-init.
# Usage: provision.sh <seed dir>
#
# The last line printed is a status sentinel that sendit looks for in the
# console log to decide whether provisioning succeeded.
set -euxo pipefail

seed="$1"
user=dev
export DEBIAN_FRONTEND=noninteractive

report() {
    local status=FAILED
    [ "$1" -eq 0 ] && status=OK
    set +x
    echo "SENDIT_PROVISION_${status}"
}
trap 'report $?' EXIT

# --- Packages ---------------------------------------------------------------
apt-get update
apt-get -y -o Dpkg::Options::=--force-confold full-upgrade
apt-get -y install --no-install-recommends \
    git curl ca-certificates build-essential pkg-config sudo \
    openssh-server cloud-guest-utils less vim-tiny
apt-get -y autoremove --purge
apt-get clean

# --- Rust -------------------------------------------------------------------
sudo -u "$user" -H bash -c \
    'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs |
         sh -s -- -y --profile minimal -c clippy -c rustfmt'

# --- Identity ---------------------------------------------------------------
hostnamectl set-hostname sendit
sed -i '/^127\.0\.1\.1\s/d' /etc/hosts
echo '127.0.1.1 sendit' >> /etc/hosts

{
    printf '\033[1;38;5;208m'
    cat "$seed/banner.txt"
    printf '\033[0m'
    echo '  Light and fast VMs. Your project is in your home directory.'
    echo '  Logging out of the console (exit or Ctrl-D) shuts the VM down.'
    echo
} > /etc/motd

# --- Console autologin ------------------------------------------------------
# Logging out of the console shuts the VM down, so `exit` ends `sendit run`
# instead of logging in again: the getty isn't restarted, and once it has
# ended, the VM powers off unless it is already shutting down or rebooting.
cat > /usr/local/sbin/sendit-console-logout <<'EOF'
#!/bin/sh
[ "$(systemctl is-system-running)" = stopping ] && exit 0
echo 'Logged out; shutting down the VM.' > /dev/hvc0 || true
exec systemctl --no-block poweroff
EOF
chmod 755 /usr/local/sbin/sendit-console-logout
mkdir -p /etc/systemd/system/serial-getty@hvc0.service.d
cat > /etc/systemd/system/serial-getty@hvc0.service.d/autologin.conf <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin $user --noclear --keep-baud 115200,57600,38400,9600 - \$TERM
Restart=no
ExecStopPost=/usr/local/sbin/sendit-console-logout
EOF

# --- Networking -------------------------------------------------------------
# cloud-init's generated config matches this VM's MAC address, but every
# project VM gets its own MAC. Replace it with a config that matches by name.
# Identifying by MAC (instead of a DUID) lets `sendit ssh` find the VM's
# lease in the host's /var/db/dhcpd_leases.
rm -f /etc/netplan/50-cloud-init.yaml \
    /etc/network/interfaces.d/50-cloud-init \
    /etc/systemd/network/10-cloud-init-*.network
cat > /etc/systemd/network/80-sendit.network <<EOF
[Match]
Name=en*

[Network]
DHCP=yes

[DHCPv4]
ClientIdentifier=mac
EOF
systemctl enable systemd-networkd.service

# --- Disk -------------------------------------------------------------------
# Project VMs get a larger (sparse) disk than the base image; grow the root
# filesystem to fill it on every boot. Freed blocks go back to the host.
cat > /usr/local/sbin/sendit-growfs <<'EOF'
#!/bin/sh
set -eu
root=$(findmnt -no SOURCE /)
name=$(basename "$root")
disk=/dev/$(lsblk -no PKNAME "$root")
part=$(cat "/sys/class/block/$name/partition")
growpart "$disk" "$part" || true   # exits 1 when there is nothing to grow
resize2fs "$root"
EOF
chmod 755 /usr/local/sbin/sendit-growfs
cat > /etc/systemd/system/sendit-growfs.service <<'EOF'
[Unit]
Description=Grow the root filesystem to fill the disk
DefaultDependencies=no
After=local-fs.target
Before=sysinit.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/sendit-growfs

[Install]
WantedBy=sysinit.target
EOF
systemctl enable sendit-growfs.service fstrim.timer

# --- Mounts -----------------------------------------------------------------
# sendit shares a manifest and the script that applies it (mount.sh) on the
# read-only sendit-meta virtiofs share. Run it before anyone can log in.
cat > /usr/local/sbin/sendit-mounts <<'EOF'
#!/bin/sh
set -eu
meta=/run/sendit/meta
mkdir -p "$meta"
mountpoint -q "$meta" || mount -t virtiofs -o ro sendit-meta "$meta"
exec sh "$meta/mount.sh" "$meta/mounts"
EOF
chmod 755 /usr/local/sbin/sendit-mounts
cat > /etc/systemd/system/sendit-mounts.service <<'EOF'
[Unit]
Description=Mount the directories shared by sendit
After=local-fs.target
Before=serial-getty@hvc0.service ssh.service systemd-user-sessions.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/sendit-mounts
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
EOF
systemctl enable sendit-mounts.service

# --- Per-VM identity --------------------------------------------------------
# Every project VM is a copy of this image, so regenerate SSH host keys and
# the machine ID on first boot instead of sharing them.
cat > /etc/systemd/system/sendit-ssh-hostkeys.service <<'EOF'
[Unit]
Description=Generate SSH host keys
Before=ssh.service
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A

[Install]
WantedBy=multi-user.target
EOF
systemctl enable sendit-ssh-hostkeys.service ssh.service

# --- Custom scripts ---------------------------------------------------------
# The user's scripts from ~/.config/sendit/provision-scripts, in name order.
# They run as the login user in a login shell from its home directory, so
# they see the same environment as the console (e.g. cargo on the PATH), and
# use sudo for anything that needs root. sudo keeps DEBIAN_FRONTEND meanwhile,
# so apt-get doesn't stop at configuration prompts.
echo 'Defaults env_keep += "DEBIAN_FRONTEND"' > /etc/sudoers.d/sendit-provision
while read -r file name; do
    echo "sendit: running custom script $name"
    (cd "/home/$user" && sudo -u "$user" -H bash -l "$seed/custom/$file") < /dev/null ||
        { echo "sendit: custom script $name failed"; exit 1; }
done < "$seed/custom/list"
rm /etc/sudoers.d/sendit-provision
apt-get clean

# cloud-init has done its job; later boots of copies must not re-run it.
touch /etc/cloud/cloud-init.disabled

rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
journalctl --rotate && journalctl --vacuum-time=1s || true
fstrim -av || true
