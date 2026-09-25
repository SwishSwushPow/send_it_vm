#!/bin/sh
# Cargo runner: ad-hoc signs the binary with the virtualization entitlement
# (required by Virtualization.framework), then runs it.
# To only sign a binary (e.g. after `cargo install`): SENDIT_SIGN_ONLY=1 scripts/sign-and-run.sh <binary>
set -e
bin="$1"
shift
entitlements="$(dirname "$0")/../entitlements.plist"
if ! out=$(codesign --sign - --force --entitlements "$entitlements" "$bin" 2>&1); then
    echo "$out" >&2
    exit 1
fi
[ -n "$SENDIT_SIGN_ONLY" ] && exit 0
exec "$bin" "$@"
