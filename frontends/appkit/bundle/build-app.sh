#!/bin/sh
# Build legcon.app from the AppKit shell (docs/PLAN.md D44), and optionally package it.
#
#   frontends/appkit/bundle/build-app.sh [--debug] [--universal] [--package VERSION] [out-dir]
#
# Builds the shell (release unless --debug; Apple silicon and Intel in one binary with
# --universal), then makes <out-dir>/legcon.app (default: frontends/appkit/target/<profile>/).
# --package also leaves legcon-macos-<arch>-VERSION.zip and .dmg there.
#
# Signing and notarization run when their credentials are present, as in vintrdp's
# scripts/dist/package-macos.sh; without them the app is signed ad hoc and not notarized:
#
#   MACOS_SIGN_IDENTITY   a "Developer ID Application: ..." identity in the keychain
#   Notarization, either an App Store Connect API key
#     NOTARY_KEY_ID, NOTARY_ISSUER_ID, NOTARY_KEY_PATH (the .p8 file)
#   or an Apple ID
#     NOTARY_APPLE_ID, NOTARY_PASSWORD (app-specific), NOTARY_TEAM_ID
set -eu
here=$(cd "$(dirname "$0")" && pwd)
shell="$here/.."
profile=release
flag=--release
universal=0
version=
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) profile=debug; flag= ;;
        --universal) universal=1 ;;
        --package) version=$2; shift ;;
        -*) echo "unknown option $1" >&2; exit 2 ;;
        *) break ;;
    esac
    shift
done
out=${1:-"$shell/target/$profile"}
mkdir -p "$out"
out=$(cd "$out" && pwd)
plist_version=${version:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$shell/Cargo.toml" | head -1)}

app="$out/legcon.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
if [ "$universal" = 1 ]; then
    arch=universal
    slices=
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
        # Intel back to High Sierra; Apple silicon starts at macOS 11 anyway.
        case $t in x86_64-*) min=10.13 ;; *) min=11.0 ;; esac
        (cd "$shell" && MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-$min}" cargo build $flag --target "$t")
        slices="$slices $shell/target/$t/$profile/legcon"
    done
    # shellcheck disable=SC2086
    lipo -create -output "$app/Contents/MacOS/legcon" $slices
else
    arch=$(uname -m)
    case $arch in x86_64) min=10.13 ;; *) min=11.0 ;; esac
    (cd "$shell" && MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-$min}" cargo build $flag)
    cp "$shell/target/$profile/legcon" "$app/Contents/MacOS/legcon"
fi
# Nothing outside the system: anything else would be missing on a user's Mac.
extra=$(otool -L "$app/Contents/MacOS/legcon" | awk '/^\t/ {print $1}' \
    | grep -v -E '^(/usr/lib/|/System/Library/)' || true)
if [ -n "$extra" ]; then
    echo "legcon links libraries macOS does not ship: $extra" >&2
    exit 1
fi
cp "$here/legcon.icns" "$app/Contents/Resources/legcon.icns"
sed "s/@VERSION@/$plist_version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"
plutil -lint "$app/Contents/Info.plist" >/dev/null

identity=${MACOS_SIGN_IDENTITY:--}
notary=
if [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER_ID:-}" ] && [ -n "${NOTARY_KEY_PATH:-}" ]; then
    notary=key
elif [ -n "${NOTARY_APPLE_ID:-}" ] && [ -n "${NOTARY_PASSWORD:-}" ] && [ -n "${NOTARY_TEAM_ID:-}" ]; then
    notary=appleid
fi
if [ "$identity" = "-" ]; then
    [ -n "$notary" ] && echo "notarization credentials set but no identity: not notarizing" >&2
    notary=
    codesign --force --sign - "$app"
else
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
    [ -z "$notary" ] && echo "signed as '$identity' but no notarization credentials: not notarizing" >&2
fi
codesign --verify --strict "$app"

# notarytool exits 0 for a rejected submission too, so read its status.
notarize() {
    [ -n "$notary" ] || return 0
    if [ "$notary" = key ]; then
        set -- "$1" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"
    else
        set -- "$1" --apple-id "$NOTARY_APPLE_ID" --password "$NOTARY_PASSWORD" --team-id "$NOTARY_TEAM_ID"
    fi
    result=$(xcrun notarytool submit "$@" --wait --output-format json)
    echo "$result"
    status=$(printf '%s' "$result" | plutil -extract status raw -o - - 2>/dev/null || true)
    if [ "$status" != Accepted ]; then
        id=$(printf '%s' "$result" | plutil -extract id raw -o - - 2>/dev/null || true)
        shift
        [ -n "$id" ] && xcrun notarytool log "$id" "$@" || true
        echo "notarization finished as '${status:-unknown}'" >&2
        exit 1
    fi
}

if [ -n "$notary" ]; then
    ditto -c -k --keepParent "$app" "$out/notarize.zip"
    notarize "$out/notarize.zip"
    rm -f "$out/notarize.zip"
    xcrun stapler staple "$app"
    spctl --assess --type execute --verbose "$app"
fi
echo "made $app ($plist_version, $arch)"

[ -n "$version" ] || exit 0
zip="$out/legcon-macos-$arch-$version.zip"
dmg="$out/legcon-macos-$arch-$version.dmg"
rm -f "$zip" "$dmg"
ditto -c -k --keepParent "$app" "$zip"
stage=$(mktemp -d)
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -volname legcon -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$stage"
if [ "$identity" != "-" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
    if [ -n "$notary" ]; then
        notarize "$dmg"
        xcrun stapler staple "$dmg"
    fi
fi
echo "made $zip and $dmg"
