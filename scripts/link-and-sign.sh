#!/bin/sh
# Linker wrapper: links with the system linker, then ad-hoc signs executables
# with the virtualization entitlement (required by Virtualization.framework).
# Set as the linker in .cargo/config.toml, so every build is signed:
# `cargo build`, `cargo run`, `cargo test` and `cargo install --path .`.
set -e
cc "$@"

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
entitlements="$(dirname "$0")/../entitlements.plist"
if ! msg=$(codesign --sign - --force --entitlements "$entitlements" "$out" 2>&1); then
    echo "$msg" >&2
    exit 1
fi
