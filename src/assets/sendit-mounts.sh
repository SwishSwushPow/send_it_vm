#!/bin/sh
# Applies sendit's mount manifest. Runs as root on every boot, started by
# sendit-mounts.service from the read-only sendit-meta share, so it always
# matches the sendit version that booted the VM.
# Usage: mount.sh <manifest>
#
# Manifest lines, applied in order:
#   share <virtiofs tag> <ro|rw> <guest path>
#   hide <guest path>     (masks a directory or file with an empty read-only one)
set -u

user=dev
status=0

fail() {
    echo "sendit-mounts: $*" >&2
    status=1
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

while read -r kind rest; do
    case $kind in
        share)
            tag=${rest%% *}
            rest=${rest#* }
            mode=${rest%% *}
            path=${rest#* }
            if mkpoint "$path" && mount -t virtiofs -o "$mode" "$tag" "$path"; then
                echo "sendit-mounts: $path ($mode)"
            else
                fail "could not mount $tag at $path"
            fi
            ;;
        hide)
            path=$rest
            if [ -d "$path" ]; then
                mount -t tmpfs -o ro,mode=0555,size=4k sendit-hidden "$path" ||
                    fail "could not hide $path"
            elif [ -e "$path" ]; then
                # A file, e.g. the .git file of a worktree.
                mount --bind -o ro /dev/null "$path" || fail "could not hide $path"
            fi
            ;;
        '' | '#'*) ;;
        *) fail "unknown manifest line: $kind $rest" ;;
    esac
done < "$1"

exit $status
