#!/bin/sh
# Builds "Database Manager.app" from a release build.
# Usage: scripts/bundle-macos.sh [output-dir]   (default: target/release)
set -eu
cd "$(dirname "$0")/.."

out="${1:-target/release}"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
app="$out/Database Manager.app"

cargo build --release -p database-manager

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/database-manager "$app/Contents/MacOS/database-manager"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>Database Manager</string>
    <key>CFBundleDisplayName</key><string>Database Manager</string>
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

# Ad-hoc signature: enough to run locally; distribution needs a Developer ID.
codesign --force --sign - "$app"
echo "Built $app"
