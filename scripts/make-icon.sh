#!/bin/sh
# Regenerates resources/AppIcon.icns from resources/icon/AppIcon.svg.
# On Linux without iconutil, render with rsvg-convert and write the .icns with Pillow.
# Run it after editing the SVG and commit the result; bundle.sh only copies it.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

swiftc -O "$repo_root/scripts/render-svg.swift" -o "$work/render-svg"
iconset="$work/AppIcon.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
    "$work/render-svg" "$repo_root/resources/icon/AppIcon.svg" "$iconset/icon_${size}x${size}.png" "$size"
    "$work/render-svg" "$repo_root/resources/icon/AppIcon.svg" "$iconset/icon_${size}x${size}@2x.png" $((size * 2))
done
iconutil --convert icns --output "$repo_root/resources/AppIcon.icns" "$iconset"
printf 'Wrote %s\n' "$repo_root/resources/AppIcon.icns"
