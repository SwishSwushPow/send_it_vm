#!/bin/bash
# Provisions the sendit base image. Runs once, as root, from cloud-init.
# Usage: provision.sh <seed dir>
#
# The last line printed is a status sentinel that sendit looks for in the
# console log to decide whether provisioning succeeded. It ends in a token
# that sendit makes up for each build, so that nothing else printing to the
# console, such as a custom script, can produce it by accident.
set -euxo pipefail

seed="$1"
user=dev
export DEBIAN_FRONTEND=noninteractive
read -r token < "$seed/token"

report() {
    local status=FAILED
    [ "$1" -eq 0 ] && status=OK
    set +x
    # cloud-init's power_state powers the VM off once this script is done,
    # but only if cloud-init is still running then: systemd stops its unit
    # (but not this script) when the OOM killer kills any process in it,
    # e.g. a compiler in a custom script. Should the VM still be up
    # 15 seconds later, it powers off anyway.
    systemd-run --quiet --on-active=15 systemctl --no-block poweroff || true
    echo "SENDIT_PROVISION_${status} $token"
}
trap 'report $?' EXIT

# --- Packages ---------------------------------------------------------------
apt-get update
apt-get -y -o Dpkg::Options::=--force-confold full-upgrade
apt-get -y install --no-install-recommends \
    git curl ca-certificates build-essential pkg-config cmake sudo \
    openssh-server cloud-guest-utils less vim-tiny \
    unzip jq ripgrep fd-find htop
# The kernel with 16 KiB pages replaces the cloud image's kernel (4 KiB
# pages): under memory pressure on the Mac, guests running that kernel lost
# writes to their memory (page cache, kernel module code, page tables), and
# none running this one have so far. Purging the running kernel would stop
# at a question about it; it is only replaced on disk, the next boot runs
# the new one.
apt-get -y install --no-install-recommends linux-image-arm64-16k
echo 'linux-base linux-base/removing-running-kernel boolean false' | debconf-set-selections
mapfile -t old_kernels < <(dpkg-query -W -f '${db:Status-Abbrev} ${Package}\n' 'linux-image-*' |
    awk '$1 == "ii" && $2 !~ /-16k$/ { print $2 }')
apt-get -y purge "${old_kernels[@]}"
kernels=(/boot/vmlinuz-*)
if [ "${#kernels[@]}" -ne 1 ] || [[ ${kernels[0]} != *-16k ]]; then
    echo "sendit: expected only the 16 KiB-page kernel, found: ${kernels[*]}"
    exit 1
fi
# Updates come with a rebuilt base image, not in the background: an upgrade
# running in the background holds up shutting down for up to 30 minutes, and
# would be cut short when sendit forces the VM off.
apt-get -y purge unattended-upgrades
systemctl disable apt-daily.timer apt-daily-upgrade.timer
apt-get -y autoremove --purge
apt-get clean
# Debian installs fd as fdfind because fdclone's file manager is also called
# fd; fdclone isn't installed here.
ln -sf /usr/bin/fdfind /usr/local/bin/fd

# --- Identity ---------------------------------------------------------------
hostnamectl set-hostname sendit
sed -i '/^127\.0\.1\.1\s/d' /etc/hosts
echo '127.0.1.1 sendit' >> /etc/hosts

# --- Console autologin ------------------------------------------------------
# Logging out of the console shuts the VM down, so `exit` ends `sendit run`
# instead of logging in again: the getty isn't restarted, and once it has
# ended, the VM powers off unless it is already shutting down or rebooting.
cat > /usr/local/sbin/sendit-console-logout <<'EOF'
#!/bin/sh
[ "$(systemctl is-system-running)" = stopping ] && exit 0
echo 'Logged out; shutting down the VM.' > /dev/hvc0 || true
exec systemctl --no-block --no-wall poweroff
EOF
chmod 755 /usr/local/sbin/sendit-console-logout
mkdir -p /etc/systemd/system/serial-getty@hvc0.service.d
# The getty waits for the host's terminal type and size (see Console terminal
# below). With TTYReset, systemd would also ask the terminal for its size
# with ANSI queries, but gives up on the reply after 333ms; the host's
# terminal is often slower to answer while the guest boots, and a late reply
# ends up as input to the shell, garbling the first command line.
cat > /etc/systemd/system/serial-getty@hvc0.service.d/autologin.conf <<EOF
[Unit]
Wants=sendit-console-terminal.service
After=sendit-console-terminal.service

[Service]
EnvironmentFile=-/run/sendit-console.env
ExecStart=
ExecStart=-/sbin/agetty --autologin $user --noclear --keep-baud 115200,57600,38400,9600 - \$TERM
TTYReset=no
Restart=no
ExecStopPost=/usr/local/sbin/sendit-console-logout
EOF

# --- Console terminal -------------------------------------------------------
# The console can't tell what the host's terminal is. Asked with a "?" over
# /dev/hvc2, sendit answers with the size as a "<rows> <cols>" line and a
# "term <TERM> [<COLORTERM>]" line, and it sends the size again whenever the
# terminal is resized. The type goes to /run/sendit-console.env for the
# getty, which waits until it is there; the size is applied to the console,
# which then tells the programs running there (SIGWINCH).
cat > /usr/local/sbin/sendit-console-terminal <<'EOF'
#!/bin/bash
# stdin and stdout are /dev/hvc2.
set -u
env=/run/sendit-console.env

apply_size() {
    case $1$2 in
        '' | *[!0-9]*) return ;;
    esac
    stty -F /dev/hvc0 rows "$1" cols "$2"
}

# Prints $1 if it looks like a terminal name.
terminal_name() {
    case $1 in
        *[!A-Za-z0-9._+-]*) ;;
        *) printf '%s' "$1" ;;
    esac
}

# Terminal types without a terminfo entry here (such as xterm-ghostty) would
# break programs, so they fall back to xterm-256color.
write_env() {
    local term colorterm
    term=$(terminal_name "$1")
    colorterm=$(terminal_name "$2")
    if [ -z "$term" ] || ! infocmp "$term" > /dev/null 2>&1; then
        term=xterm-256color
    fi
    {
        echo "TERM=$term"
        [ -z "$colorterm" ] || echo "COLORTERM=$colorterm"
    } > "$env.tmp"
    mv "$env.tmp" "$env"
}

# The kernel forgets the size whenever nothing has the console open, e.g.
# before the getty has started. Hold it open from a child: this script leads
# its session, so opening the console would make it its controlling terminal,
# and the getty couldn't have it anymore.
sleep infinity < /dev/hvc0 &
stty -echo
echo '?'
# The getty waits for the type, so don't wait long for it.
term= colorterm=
while read -r -t 5 first second third; do
    if [ "$first" = term ]; then
        term=$second colorterm=$third
        break
    fi
    apply_size "$first" "$second"
done
write_env "$term" "$colorterm"
systemd-notify --ready
while read -r first second third; do
    [ "$first" = term ] || apply_size "$first" "$second"
done
EOF
chmod 755 /usr/local/sbin/sendit-console-terminal
cat > /etc/systemd/system/sendit-console-terminal.service <<'EOF'
[Unit]
Description=Apply the host terminal's type and size to the console
BindsTo=dev-hvc2.device
After=dev-hvc2.device

[Service]
Type=notify
# systemd-notify runs as a child of the script.
NotifyAccess=all
ExecStart=/usr/local/sbin/sendit-console-terminal
# systemd opens these without making /dev/hvc2 the controlling terminal.
StandardInput=file:/dev/hvc2
StandardOutput=file:/dev/hvc2
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF
systemctl enable sendit-console-terminal.service
# When the VM starts shutting down, however that came about, a "!" on
# /dev/hvc2 tells sendit, which forces the VM off if the shutdown hangs.
# Nothing else here is ordered before this, so it runs right away.
cat > /etc/systemd/system/sendit-shutdown-notice.service <<'EOF'
[Unit]
Description=Tell sendit that the VM is shutting down
DefaultDependencies=no
Before=shutdown.target

[Service]
Type=oneshot
ExecStart=/bin/echo !
StandardOutput=file:/dev/hvc2

[Install]
WantedBy=poweroff.target halt.target
EOF
systemctl enable sendit-shutdown-notice.service
cat > /etc/profile.d/sendit-terminal.sh <<'EOF'
# login keeps only TERM from the getty's environment; bring back COLORTERM.
if [ "$(tty)" = /dev/hvc0 ] && [ -r /run/sendit-console.env ]; then
    set -a
    . /run/sendit-console.env
    set +a
fi
# ssh passes the host's TERM through, which may have no terminfo entry here
# (such as xterm-ghostty); fall back like the console does.
if [ -n "${TERM:-}" ] && ! infocmp "$TERM" > /dev/null 2>&1; then
    export TERM=xterm-256color
fi
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

# The host is the NAT's gateway. Its subnet belongs to macOS's vmnet and can
# change, so give the host a stable name instead, updated on every boot.
cat > /usr/local/sbin/sendit-host-name <<'EOF'
#!/bin/sh
set -eu
gw=$(ip -4 route show default | awk '{ print $3; exit }')
sed -i '/\shost\.sendit\.internal$/d' /etc/hosts
[ -n "$gw" ] && echo "$gw host.sendit.internal" >> /etc/hosts
exit 0
EOF
chmod 755 /usr/local/sbin/sendit-host-name
cat > /etc/systemd/system/sendit-host-name.service <<'EOF'
[Unit]
Description=Point host.sendit.internal at the host
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/sendit-host-name

[Install]
WantedBy=multi-user.target
EOF
systemctl enable sendit-host-name.service

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
# growpart exits 1 when there is nothing to grow, and 2 when it fails.
growpart "$disk" "$part" || [ $? -eq 1 ]
# Like systemd-growfs-root.service, which the image's fstab asks for
# (x-systemd.growfs) but which runs before the partition has grown. Not
# resize2fs: with 16 KiB pages it rounds the size down to whole pages, and
# once systemd has grown the filesystem to the partition's end, that would
# mean shrinking it, which fails.
/usr/lib/systemd/systemd-growfs /
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
# read-only sendit-meta virtiofs share. Run it before anyone can log in, and
# before anything else that could run the login user's code: mount.sh checks
# paths in the user's home for symlinks, which only holds while nothing can
# plant one meanwhile. User services (lingering) wait for user sessions;
# cron and atd, should a custom script install them, wait for this.
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
Before=serial-getty@hvc0.service ssh.service systemd-user-sessions.service cron.service atd.service

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

# --- Root access ------------------------------------------------------------
# Only the host becomes root, over SSH with a key of its own (`sendit ssh
# --root`). The login user loses its sudo rights at the end of
# provisioning, so nothing running in the VM can become root, e.g. to
# unmount the mask over the project's .git.
install -d -m 700 /root/.ssh
install -m 600 "$seed/root.pub" /root/.ssh/authorized_keys
echo 'PermitRootLogin prohibit-password' > /etc/ssh/sshd_config.d/sendit.conf
# Debian's root .bashrc has no colours; add those of /etc/skel/.bashrc and
# its window title, with a red prompt so a root shell stands out from the
# login user's.
cat >> /root/.bashrc <<'EOF'

# Added by sendit: colours and window title as in /etc/skel/.bashrc, with a
# red prompt.
case $- in
    *i*) ;;
    *) return ;;
esac
case "$TERM" in
    xterm-color | *-256color)
        PS1='${debian_chroot:+($debian_chroot)}\[\033[01;31m\]\u@\h\[\033[00m\]:\[\033[01;34m\]\w\[\033[00m\]\$ '
        ;;
esac
case "$TERM" in
    xterm* | rxvt*)
        PS1="\[\e]0;${debian_chroot:+($debian_chroot)}\u@\h: \w\a\]$PS1"
        ;;
esac
if [ -x /usr/bin/dircolors ]; then
    eval "$(dircolors -b)"
    alias ls='ls --color=auto'
    alias grep='grep --color=auto'
fi
EOF

# --- Custom scripts ---------------------------------------------------------
# The user's scripts from ~/.config/sendit/provision-scripts, in name order.
# They run as the login user in a login shell from its home directory, so
# they see the same environment as the console (e.g. PATH changes earlier
# scripts made in ~/.profile), and use sudo for anything that needs root.
# sudo keeps DEBIAN_FRONTEND meanwhile, so apt-get doesn't stop at
# configuration prompts.
echo 'Defaults env_keep += "DEBIAN_FRONTEND"' > /etc/sudoers.d/sendit-provision
while read -r file name; do
    echo "sendit: running custom script $name"
    (cd "/home/$user" && sudo -u "$user" -H bash -l "$seed/custom/$file") < /dev/null ||
        { echo "sendit: custom script $name failed"; exit 1; }
done < "$seed/custom/list"
rm /etc/sudoers.d/sendit-provision
apt-get clean

# --- No more sudo -----------------------------------------------------------
# The custom scripts were the last to need it (see Root access above).
rm -f /etc/sudoers.d/90-cloud-init-users
gpasswd -d "$user" sudo
if sudo -l -U "$user" true > /dev/null 2>&1; then
    echo "sendit: $user can still use sudo"
    exit 1
fi

# cloud-init has done its job; later boots of copies must not re-run it.
touch /etc/cloud/cloud-init.disabled

rm -f /etc/ssh/ssh_host_*
truncate -s 0 /etc/machine-id
rm -f /var/lib/dbus/machine-id
journalctl --rotate && journalctl --vacuum-time=1s || true
fstrim -av || true
