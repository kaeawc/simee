#!/usr/bin/env bash
# Builds the desktop window as a macOS app bundle with simee's icon (assets/brand/macos/simee.icns),
# so the Dock and Finder show the mark: desktop/target/release/simee.app.
#
#   scripts/desktop-app.sh          # build the bundle and print its path
#   scripts/desktop-app.sh --open   # and open it
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release --manifest-path "$root/desktop/Cargo.toml"

release=$root/desktop/target/release
app=$release/simee.app
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/desktop/Cargo.toml" | head -n 1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$release/simee-desktop" "$app/Contents/MacOS/simee-desktop"
cp "$root/assets/brand/macos/simee.icns" "$app/Contents/Resources/simee.icns"
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>simee</string>
  <key>CFBundleDisplayName</key><string>simee</string>
  <key>CFBundleIdentifier</key><string>dev.simee.desktop</string>
  <key>CFBundleExecutable</key><string>simee-desktop</string>
  <key>CFBundleIconFile</key><string>simee</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
echo "$app"
if [[ ${1:-} == --open ]]; then
  open "$app"
fi
