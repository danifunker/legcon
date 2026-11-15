#!/bin/sh
# Build legcon.app from the AppKit shell (docs/PLAN.md D44).
#
#   frontends/appkit/bundle/build-app.sh [--debug] [out-dir]
#
# Builds the shell (release unless --debug), then makes <out-dir>/legcon.app (default:
# frontends/appkit/target/<profile>/) with Info.plist, the icon and the binary, signed ad hoc so
# it runs on Apple silicon. Developer ID signing and notarization come later.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
shell="$here/.."
profile=release
flag=--release
if [ "${1:-}" = "--debug" ]; then
    profile=debug
    flag=
    shift
fi
(cd "$shell" && cargo build $flag)
out=${1:-"$shell/target/$profile"}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$shell/Cargo.toml" | head -1)
app="$out/legcon.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$shell/target/$profile/legcon" "$app/Contents/MacOS/legcon"
cp "$here/legcon.icns" "$app/Contents/Resources/legcon.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - "$app"
codesign --verify --strict "$app"
echo "made $app ($version)"
