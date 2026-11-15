#!/bin/sh
# Make the app icon from make-icon.swift: icon-512.png (the shell embeds it) and legcon.icns.
# Run by hand when the drawing changes; both outputs are committed.
set -eu
cd "$(dirname "$0")"
work=$(mktemp -d)
swift make-icon.swift "$work/icon-1024.png"
set_dir="$work/legcon.iconset"
mkdir "$set_dir"
for s in 16 32 128 256 512; do
    sips -z "$s" "$s" "$work/icon-1024.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    d=$((s * 2))
    sips -z "$d" "$d" "$work/icon-1024.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o legcon.icns
cp "$set_dir/icon_512x512.png" icon-512.png
rm -rf "$work"
echo "made legcon.icns and icon-512.png"
