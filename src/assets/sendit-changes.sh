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

# One descriptor stays open on the port from here until cat takes it over:
# the hvc driver resets the port's termios (back to cooked, echoing) when the
# last descriptor closes, so each separate open would undo the settings below.
exec 3<> /dev/hvc3
# Raw and silent, so that any byte of a path passes unchanged and nothing is
# echoed back to the host.
stty -F /dev/hvc3 raw -echo
# Discard what the port holds from before this start: records the host sent
# while nothing read them, which the cooked port may have mangled. The host
# sends those paths again once it reads the `?` below. cat ends once nothing
# has come for 0.2 s (min 0 time 2); --foreground keeps it in the port's
# foreground process group, so that it may read.
stty -F /dev/hvc3 min 0 time 2
timeout --foreground 3 cat <&3 > /dev/null || true
stty -F /dev/hvc3 min 1 time 0
# The module gives each open file its own framing and root, which must come
# first: the root and the records go through the same one.
exec 4> /dev/sendit-notify
printf 'R 0 %s\0' "$root" >&4
# Tell the host that records are accepted now; it holds them back until then.
printf '?' >&3
exec cat <&3 >&4
