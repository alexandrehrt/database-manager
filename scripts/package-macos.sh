#!/bin/sh
# Builds a shareable Cuia disk image: a universal (Apple Silicon + Intel) app
# for macOS 11 or later, with an Applications shortcut and first-run notes.
# Usage: scripts/package-macos.sh [output-dir]   (default: target/dist)
#
# Without a Developer ID the app is signed ad hoc, and macOS asks the person
# installing it to approve it once (see the notes inside the image). With an
# Apple Developer account, set:
#   CUIA_SIGN_IDENTITY   "Developer ID Application: Name (TEAMID)"
#   CUIA_NOTARY_PROFILE  a notarytool keychain profile
#                        (xcrun notarytool store-credentials …)
# and the image is signed, notarized and stapled, so it opens without warnings.
set -eu
cd "$(dirname "$0")/.."

out="${1:-target/dist}"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
export MACOSX_DEPLOYMENT_TARGET=11.0

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target add "$target" >/dev/null
    cargo build --release -p database-manager --target "$target"
done
mkdir -p target/universal
lipo -create -output target/universal/database-manager \
    target/aarch64-apple-darwin/release/database-manager \
    target/x86_64-apple-darwin/release/database-manager

stage=target/dmg-stage
rm -rf "$stage"
mkdir -p "$stage"
CUIA_BINARY=target/universal/database-manager scripts/bundle-macos.sh "$stage"
ln -s /Applications "$stage/Applications"
cp scripts/macos-readme.txt "$stage/Read me first.txt"

mkdir -p "$out"
dmg="$out/Cuia-$version-macOS.dmg"
rm -f "$dmg"
hdiutil create -volname "Cuia" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg" >/dev/null

if [ -n "${CUIA_SIGN_IDENTITY:-}" ]; then
    codesign --force --timestamp --sign "$CUIA_SIGN_IDENTITY" "$dmg"
    if [ -n "${CUIA_NOTARY_PROFILE:-}" ]; then
        xcrun notarytool submit "$dmg" --keychain-profile "$CUIA_NOTARY_PROFILE" --wait
        xcrun stapler staple "$dmg"
    fi
fi
echo "Built $dmg"
