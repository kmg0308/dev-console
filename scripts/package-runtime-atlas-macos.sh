#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_INPUT="${1:-}"

if [[ "$(uname -s)" != "Darwin" || -z "$APP_INPUT" ]]; then
    echo "usage: $0 /path/to/RuntimeAtlas.app [output.pkg]" >&2
    exit 64
fi

APP_DIR="$(cd "$(dirname "$APP_INPUT")" && pwd -P)/$(basename "$APP_INPUT")"
INFO_PLIST="$APP_DIR/Contents/Info.plist"
MAIN="$APP_DIR/Contents/MacOS/RuntimeAtlas"
CLI="$APP_DIR/Contents/MacOS/runtime-atlas"
SUPERVISOR="$APP_DIR/Contents/MacOS/runtime-atlas-supervisor"
APP_HELPER="$APP_DIR/Contents/Helpers/runtime-atlas"

if [[ ! -d "$APP_DIR" || -L "$APP_DIR" || ! -f "$INFO_PLIST" || -L "$INFO_PLIST" ]]; then
    echo "app input must be a regular non-symlink bundle" >&2
    exit 1
fi
for executable in "$MAIN" "$CLI" "$SUPERVISOR" "$APP_HELPER"; do
    if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
        echo "app executables must be regular non-symlink files" >&2
        exit 1
    fi
done
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$INFO_PLIST")" = "com.kmg0308.runtimeatlas"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$INFO_PLIST")" = "RuntimeAtlas"
APP_SIGN_IDENTITY="${APP_SIGN_IDENTITY:--}"
if [[ -n "${INSTALLER_SIGN_IDENTITY:-}" && "$APP_SIGN_IDENTITY" = "-" ]]; then
    echo "INSTALLER_SIGN_IDENTITY requires a Developer ID Application APP_SIGN_IDENTITY" >&2
    exit 1
fi
"$ROOT_DIR/scripts/verify-runtime-atlas-macos-executables.sh" "$CLI" "$SUPERVISOR" "$APP_HELPER"

VERSION="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$INFO_PLIST")"
if [[ ! "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,3}$ ]]; then
    echo "unsupported app version: $VERSION" >&2
    exit 1
fi

OUTPUT="${2:-$ROOT_DIR/dist/RuntimeAtlas-$VERSION.pkg}"
if [[ -e "$OUTPUT" ]]; then
    echo "refusing to overwrite: $OUTPUT" >&2
    exit 1
fi
mkdir -p "$(dirname "$OUTPUT")"

STAGE="$(mktemp -d "${TMPDIR:-/tmp}/runtime-atlas-package.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
PAYLOAD="$STAGE/payload"
STAGED_APP="$PAYLOAD/Applications/RuntimeAtlas.app"
mkdir -p "$PAYLOAD/Applications" "$PAYLOAD/usr/local/bin"
ditto "$APP_DIR" "$STAGED_APP"
xattr -cr "$STAGED_APP"

SIGN_ARGS=(--force --sign "$APP_SIGN_IDENTITY")
if [[ "$APP_SIGN_IDENTITY" != "-" ]]; then
    SIGN_ARGS+=(--options runtime --timestamp)
fi
if [[ ! -d "$STAGED_APP" || -L "$STAGED_APP" || ! -f "$STAGED_APP/Contents/Info.plist" || -L "$STAGED_APP/Contents/Info.plist" ]]; then
    echo "staged app must be a regular non-symlink bundle" >&2
    exit 1
fi
for executable in \
    "$STAGED_APP/Contents/MacOS/RuntimeAtlas" \
    "$STAGED_APP/Contents/MacOS/runtime-atlas" \
    "$STAGED_APP/Contents/MacOS/runtime-atlas-supervisor" \
    "$STAGED_APP/Contents/Helpers/runtime-atlas"; do
    if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
        echo "staged executables must be regular non-symlink files" >&2
        exit 1
    fi
done
codesign "${SIGN_ARGS[@]}" "$STAGED_APP/Contents/MacOS/runtime-atlas"
install -m 0755 "$STAGED_APP/Contents/MacOS/runtime-atlas" "$STAGED_APP/Contents/Helpers/runtime-atlas"
codesign "${SIGN_ARGS[@]}" "$STAGED_APP/Contents/MacOS/runtime-atlas-supervisor"
codesign "${SIGN_ARGS[@]}" "$STAGED_APP"
if [[ "$APP_SIGN_IDENTITY" != "-" ]]; then
    codesign -dv --verbose=4 "$STAGED_APP" 2>&1 | grep -q '^Authority=Developer ID Application:'
fi
install -m 0755 "$STAGED_APP/Contents/MacOS/runtime-atlas" "$PAYLOAD/usr/local/bin/runtime-atlas"
if [[ ! -f "$PAYLOAD/usr/local/bin/runtime-atlas" || -L "$PAYLOAD/usr/local/bin/runtime-atlas" ]]; then
    echo "global CLI payload must be a regular non-symlink file" >&2
    exit 1
fi

cat > "$STAGE/component.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
  <dict>
    <key>BundleHasStrictIdentifier</key><true/>
    <key>BundleIsRelocatable</key><false/>
    <key>BundleIsVersionChecked</key><true/>
    <key>BundleOverwriteAction</key><string>upgrade</string>
    <key>RootRelativeBundlePath</key><string>Applications/RuntimeAtlas.app</string>
  </dict>
</array>
</plist>
PLIST

PKGBUILD_ARGS=(
    --root "$PAYLOAD"
    --component-plist "$STAGE/component.plist"
    --install-location /
    --identifier com.kmg0308.runtimeatlas.pkg
    --version "$VERSION"
    --ownership recommended
)
SIGNATURE_EXPECTATION=unsigned
if [[ -n "${INSTALLER_SIGN_IDENTITY:-}" ]]; then
    PKGBUILD_ARGS+=(--sign "$INSTALLER_SIGN_IDENTITY")
    SIGNATURE_EXPECTATION=signed
fi
pkgbuild "${PKGBUILD_ARGS[@]}" "$STAGE/RuntimeAtlas.pkg" >/dev/null
"$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$STAGE/RuntimeAtlas.pkg" "$SIGNATURE_EXPECTATION" none
mv -n "$STAGE/RuntimeAtlas.pkg" "$OUTPUT"
if [[ -e "$STAGE/RuntimeAtlas.pkg" ]]; then
    echo "refusing to overwrite: $OUTPUT" >&2
    exit 1
fi
echo "$OUTPUT"
