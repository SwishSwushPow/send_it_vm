#!/bin/bash
# Provisions the send_it base image. Runs once, as root, from cloud-init.
# Usage: provision.sh <seed dir>
#
# The last line printed is a status sentinel that send_it looks for in the
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
    echo '  Light and fast VMs. Your project is in /workspace.'
    echo
} > /etc/motd

# --- Console autologin ------------------------------------------------------
mkdir -p /etc/systemd/system/serial-getty@hvc0.service.d
cat > /etc/systemd/system/serial-getty@hvc0.service.d/autologin.conf <<EOF
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin $user --noclear --keep-baud 115200,57600,38400,9600 - \$TERM
EOF

# --- Networking -------------------------------------------------------------
# cloud-init's generated config matches this VM's MAC address, but every
# project VM gets its own MAC. Replace it with a config that matches by name.
rm -f /etc/netplan/50-cloud-init.yaml \
    /etc/network/interfaces.d/50-cloud-init \
    /etc/systemd/network/10-cloud-init-*.network
cat > /etc/systemd/network/80-sendit.network <<EOF
[Match]
Name=en*

[Network]
DHCP=yes
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
# send_it shares a manifest and the script that applies it (mount.sh) on the
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
Description=Mount the directories shared by send_it
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

# cloud-init has done its job; later boots of copies must not re-run it.
touch /etc/cloud/cloud-init.disabled

rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
journalctl --rotate && journalctl --vacuum-time=1s || true
fstrim -av || true
