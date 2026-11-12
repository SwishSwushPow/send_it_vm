#!/bin/sh
# Builds sendit-ui and packs it as "Send it.app" in the workspace's target/,
# to start it from the Finder or the Dock. The app uses the sendit you
# installed, e.g. with `cargo install --path ../sendit-vm`; set another in its
# Preferences.
set -e
cd "$(dirname "$0")"
cargo build --release
target=${CARGO_TARGET_DIR:-../target}
app="$target/Send it.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$target/release/sendit-ui" "$app/Contents/MacOS/"
cp icon/Assets.car icon/AppIcon.icns "$app/Contents/Resources/"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Send it</string>
    <key>CFBundleDisplayName</key><string>Send it</string>
    <key>CFBundleIdentifier</key><string>io.github.swishswushpow.sendit-ui</string>
    <key>CFBundleExecutable</key><string>sendit-ui</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleIconName</key><string>AppIcon</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign --force --sign - "$app"
echo "Built $app"
