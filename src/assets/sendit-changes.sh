#!/bin/sh
# Passes file changes made on the host to the watchers of the project in the
# guest. sendit-changes (in the base image) runs this from the meta share as
# root once /dev/hvc3, the host's change channel, exists.
set -eu

root=$(sed -n 's/^workdir //p' /run/sendit/meta/mounts)
if [ -z "$root" ]; then
    echo "sendit-changes: the manifest names no project directory" >&2
    exit 0
fi

# Raw and silent, so that any byte of a path passes unchanged and nothing is
# echoed back to the host.
stty -F /dev/hvc3 raw -echo
# The module needs the project root before any other record.
printf 'R 0 %s\0' "$root" > /dev/sendit-notify
# Tell the host that records are accepted now; it drops batches until then.
printf '?' > /dev/hvc3
exec cat /dev/hvc3 > /dev/sendit-notify
