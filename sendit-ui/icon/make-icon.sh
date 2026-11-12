#!/bin/sh
# Draws the app icon again, after a change to make-icon.swift or
# AppIcon.icon/icon.json, light and dark: AppIcon.icon's layers, and from
# them, with Xcode's actool, Assets.car and AppIcon.icns, which bundle.sh puts
# in the app; and the PNGs for the sidebar and the Dock. It changes nothing
# here unless all of that works.
set -e
cd "$(dirname "$0")"
here=$(pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/icon/AppIcon.icon" "$tmp/compiled"
cp AppIcon.icon/icon.json "$tmp/icon/AppIcon.icon/"
swift make-icon.swift "$tmp/icon"

# Assets.car holds the icon that macOS shows: light, dark or tinted from
# macOS 26 on, flat before. AppIcon.icns is the fallback that
# CFBundleIconFile names, for whatever reads icons the old way.
# actool reports its problems on stdout and may still exit with 0. It takes
# relative paths as relative to where it first ran, so the paths are absolute.
report=$(xcrun actool "$tmp/icon/AppIcon.icon" --compile "$tmp/compiled" \
    --platform macosx --minimum-deployment-target 13.0 --app-icon AppIcon \
    --output-partial-info-plist "$tmp/compiled/partial.plist" \
    --errors --warnings --output-format human-readable-text 2>&1) || {
    printf '%s\n' "$report" >&2
    exit 1
}
if printf '%s\n' "$report" | grep -Eq 'com\.apple\.actool\.(errors|warnings)|: (error|warning): '; then
    printf '%s\n' "$report" >&2
    exit 1
fi

cp "$tmp"/icon/AppIcon.icon/Assets/*.png AppIcon.icon/Assets/
cp "$tmp"/icon/*.png "$tmp/compiled/Assets.car" "$tmp/compiled/AppIcon.icns" .
echo "Drew the icon in $here"
