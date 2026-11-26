#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
app_dir="$repo_root/dist/GitSync.app"
profile=${GITSYNC_BUILD_PROFILE:-dev}
# "-" signs ad hoc. Set a "Developer ID Application: …" identity for a release.
sign_identity=${GITSYNC_SIGN_IDENTITY:--}

cd "$repo_root"
case "$profile" in
    dev)
        cargo build
        binary="$repo_root/target/debug/gitsync"
        ;;
    release)
        cargo build --release
        binary="$repo_root/target/release/gitsync"
        ;;
    *)
        printf 'Unsupported GITSYNC_BUILD_PROFILE: %s\n' "$profile" >&2
        exit 2
        ;;
esac

rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
cp "$binary" "$app_dir/Contents/MacOS/gitsync"
cp "$repo_root/resources/Info.plist" "$app_dir/Contents/Info.plist"
cp "$repo_root/resources/AppIcon.icns" "$app_dir/Contents/Resources/AppIcon.icns"

# Cargo.toml is the only version source. The build number is the commit count,
# so it always goes up (1 outside a Git checkout).
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
build=$(git rev-list --count HEAD 2>/dev/null || printf '1')
plist="$app_dir/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleShortVersionString string $version" "$plist"
/usr/libexec/PlistBuddy -c "Add :CFBundleVersion string $build" "$plist"

if command -v cargo-about >/dev/null 2>&1; then
    cargo about generate --fail about.hbs \
        -o "$app_dir/Contents/Resources/THIRD_PARTY_LICENSES.html"
else
    printf 'cargo-about is not installed; the bundle has no THIRD_PARTY_LICENSES.html\n' >&2
fi

if [ "$sign_identity" = "-" ]; then
    codesign --force --deep --sign - "$app_dir"
else
    codesign --force --deep --options runtime --timestamp \
        --sign "$sign_identity" "$app_dir"
fi

printf 'Built %s\n' "$app_dir"
