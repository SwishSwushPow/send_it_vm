#!/bin/sh
# Linker wrapper: links with the system linker, then ad-hoc signs executables
# with the virtualization entitlement (required by Virtualization.framework).
# Set as the linker in .cargo/config.toml, so every build of sendit is signed:
# `cargo build`, `cargo run`, `cargo test` and `cargo install --path sendit-vm`.
# sendit-ui, the other member of the workspace, is linked without it.
set -e
cc "$@"

# sendit-ui, the desktop app in this workspace, runs no VMs: link it plainly.
# rustc passes cargo's environment on to the linker.
[ "${CARGO_PKG_NAME-}" = sendit-ui ] && exit 0

# Find the output path. rustc may pass arguments in @response files, one per line.
out=
take=
for arg in "$@"; do
    case $arg in
        @*) args=$(cat "${arg#@}") ;;
        *) args=$arg ;;
    esac
    # Arguments inside a response file are newline-separated.
    while IFS= read -r a; do
        if [ -n "$take" ]; then
            out=$a
            take=
        elif [ "$a" = "-o" ]; then
            take=1
        fi
    done <<EOF
$args
EOF
done

case $out in
    '' | *.dylib | *.so) exit 0 ;;
esac
entitlements="$(dirname "$0")/../sendit-vm/entitlements.plist"
if ! msg=$(codesign --sign - --force --entitlements "$entitlements" "$out" 2>&1); then
    echo "$msg" >&2
    exit 1
fi
