#!/bin/sh
# Builds target/FROG.app, a universal app bundle for Apple Silicon and Intel Macs, and zips it
# as target/FROG-macos-universal.zip. Runs on macOS, with both Rust targets installed:
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# The bundle has an ad-hoc signature only. Apple Silicon won't run unsigned code, and the
# signature must match the bundle's contents, but it proves nothing about who built it.
# Gatekeeper still warns on first open until the app is signed with a Developer ID and
# notarized.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

# The oldest macOS the binary supports. Matches LSMinimumSystemVersion in Info.plist.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}"

# The package's version: the first version line in Cargo.toml, under [package].
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
if [ -z "$version" ]; then
    echo "Could not read the version from Cargo.toml." >&2
    exit 1
fi

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --locked --target "$target"
done

app=target/FROG.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
lipo -create -output "$app/Contents/MacOS/frog" \
    target/aarch64-apple-darwin/release/frog \
    target/x86_64-apple-darwin/release/frog
install -m 644 assets/frog.icns "$app/Contents/Resources/frog.icns"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"
lipo -info "$app/Contents/MacOS/frog"

rm -f target/FROG-macos-universal.zip
ditto -c -k --keepParent "$app" target/FROG-macos-universal.zip
echo "Built $root/target/FROG-macos-universal.zip (version $version)"
