#!/bin/sh
# Applies sendit's network rules: the ruleset sendit wrote next to this
# script, with the NAT's gateway (the Mac) added to its gateway4 and
# gateway6 sets, in one transaction that replaces the sendit table.
# Runs as root once the network is up, started by sendit-network.service
# from the read-only sendit-meta share, so it always matches the sendit
# version that booted the VM. Until it succeeds, the rules the base image
# loads before the network comes up stay: no connections out of the VM but
# DHCP and IPv6's neighbour and router discovery.
# Usage: network.sh <ruleset>
set -u

# The address after "via" in the first default route of family $1.
gateway() {
    ip "$1" route show default | awk '{
        for (i = 1; i < NF; i++) if ($i == "via") { print $(i + 1); exit }
    }'
}

# The gateway is in the sets only as it is now, so wait for it a while
# should the network be up without a default route yet. Without one, DNS
# and the Mac stay out of reach until the next boot.
tries=0
while gw4=$(gateway -4); [ -z "$gw4" ] && [ "$tries" -lt 30 ]; do
    sleep 1
    tries=$((tries + 1))
done
# IPv6 may not have its router yet; DNS then goes over IPv4.
gw6=$(gateway -6)
{
    cat "$1"
    case $gw4 in
        '' | *[!0-9.]*) ;;
        *) echo "add element inet sendit gateway4 { $gw4 }" ;;
    esac
    case $gw6 in
        '' | *[!0-9a-f:]*) ;;
        *) echo "add element inet sendit gateway6 { $gw6 }" ;;
    esac
} | nft -f - || {
    echo "sendit-network: could not apply the network rules; nothing gets out of the VM" >&2
    exit 1
}
sed -n '1s/^# sendit-network: /sendit-network: /p' "$1"
if [ -z "$gw4" ]; then
    echo "sendit-network: no IPv4 default route; DNS and the Mac are out of reach" \
        "until the VM starts again" >&2
    exit 1
fi
