#!/bin/sh
# Applies sendit's mount manifest and installs the login message next to it.
# Runs as root on every boot, started by sendit-mounts.service from the
# read-only sendit-meta share, so it always matches the sendit version that
# booted the VM.
# Usage: mount.sh <manifest>
#
# Manifest lines, applied in order:
#   share <virtiofs tag> <ro|rw> <guest path>
#   hide <guest path>     (masks a directory or file with an empty read-only one)
#   workdir <guest path>  (where interactive login shells start)
#   timezone <name>       (the host's time zone, e.g. Europe/Berlin)
set -u

user=dev
status=0
profile=/etc/profile.d/sendit-workdir.sh

fail() {
    echo "sendit-mounts: $*" >&2
    status=1
}

# Whether $1 or one of its parents is a symlink. The user may have planted
# one below its home to make this script, which runs as root, mount or
# chown something elsewhere. Expects a normalized absolute path.
has_symlink() {
    p=$1
    while [ "$p" != / ]; do
        [ -L "$p" ] && return 0
        p=$(dirname "$p")
    done
    return 1
}

# Creates a mount point and any missing parents. Directories created inside
# the user's home belong to the user, so e.g. ~/.cache stays writable.
mkpoint() {
    [ -d "$1" ] && return 0
    mkpoint "$(dirname "$1")" || return 1
    mkdir "$1" || return 1
    case $1 in
        "/home/$user"/*) chown "$user:" "$1" ;;
    esac
}

rm -f "$profile"
cp "$(dirname "$1")/motd" /etc/motd || fail "could not write /etc/motd"

while read -r kind rest; do
    case $kind in
        share)
            tag=${rest%% *}
            rest=${rest#* }
            mode=${rest%% *}
            path=${rest#* }
            if has_symlink "$path"; then
                fail "not mounting $tag at $path: it leads through a symlink"
            elif mkpoint "$path" && mount -t virtiofs -o "$mode" "$tag" "$path"; then
                echo "sendit-mounts: $path ($mode)"
            else
                fail "could not mount $tag at $path"
            fi
            ;;
        hide)
            path=$rest
            if has_symlink "$path"; then
                fail "not hiding $path: it leads through a symlink"
            elif [ -d "$path" ]; then
                mount -t tmpfs -o ro,mode=0555,size=4k sendit-hidden "$path" ||
                    fail "could not hide $path"
            elif [ -e "$path" ]; then
                # A file, e.g. the .git file of a worktree.
                mount --bind -o ro /dev/null "$path" || fail "could not hide $path"
            fi
            ;;
        workdir)
            # Single-quote the path for the profile script.
            quoted=$(printf '%s' "$rest" | sed "s/'/'\\\\''/g")
            cat > "$profile" <<EOF || fail "could not write $profile"
# Written by sendit-mounts on boot: start login shells in the project.
[ "\$PWD" = "\$HOME" ] && cd '$quoted' 2>/dev/null
EOF
            ;;
        timezone)
            # Like timedatectl set-timezone, which needs systemd-timedated
            # and D-Bus, neither of which is up yet. systemd notices the
            # change; login shells, which start after this, see the new zone.
            zone=/usr/share/zoneinfo/$rest
            if [ -f "$zone" ]; then
                ln -sf "$zone" /etc/localtime || fail "could not set the time zone to $rest"
            else
                fail "unknown time zone $rest; staying on $(readlink /etc/localtime)"
            fi
            ;;
        '' | '#'*) ;;
        *) fail "unknown manifest line: $kind $rest" ;;
    esac
done < "$1"

exit $status
