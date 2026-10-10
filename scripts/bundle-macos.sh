#!/bin/sh
# Builds "Cuia.app" from a release build.
# Usage: scripts/bundle-macos.sh [output-dir]   (default: target/release)
#
# CUIA_BINARY: use this executable instead of building one (the packaging
#   script passes a universal arm64 + x86_64 build).
# CUIA_SIGN_IDENTITY: a "Developer ID Application: …" identity to sign with
#   (hardened runtime, as notarization requires); unset means an ad-hoc signature.
set -eu
cd "$(dirname "$0")/.."

out="${1:-target/release}"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app="$out/Cuia.app"

binary="${CUIA_BINARY:-}"
if [ -z "$binary" ]; then
    cargo build --release -p database-manager
    binary=target/release/database-manager
fi

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/database-manager"
cp crates/app/assets/icon/cuia.icns "$app/Contents/Resources/cuia.icns"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Cuia</string>
    <key>CFBundleDisplayName</key><string>Cuia</string>
    <key>CFBundleIconFile</key><string>cuia</string>
    <key>CFBundleIdentifier</key><string>io.github.alexandrehrt.database-manager</string>
    <key>CFBundleExecutable</key><string>database-manager</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
PLIST

if [ -n "${CUIA_SIGN_IDENTITY:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$CUIA_SIGN_IDENTITY" "$app"
else
    # Ad-hoc signature: runs on this Mac; other Macs ask to approve it once.
    codesign --force --sign - "$app"
fi
echo "Built $app"
