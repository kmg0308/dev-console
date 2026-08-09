#!/usr/bin/env bash
set -euo pipefail

PKG="${1:-}"
SIGNATURE_EXPECTATION="${2:-auto}"
NOTARIZATION_EXPECTATION="${3:-${NOTARIZATION_EXPECTATION:-none}}"
if [[ "$(uname -s)" != "Darwin" || ! -f "$PKG" || -L "$PKG" || ! "$SIGNATURE_EXPECTATION" =~ ^(auto|signed|unsigned)$ || ! "$NOTARIZATION_EXPECTATION" =~ ^(none|stapled)$ ]]; then
    echo "usage: $0 RuntimeAtlas.pkg [auto|signed|unsigned] [none|stapled]" >&2
    exit 64
fi

CHECK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/runtime-atlas-package-check.XXXXXX")"
trap 'rm -rf "$CHECK_DIR"' EXIT
pkgutil --expand-full "$PKG" "$CHECK_DIR/expanded" >/dev/null

PACKAGE_INFO="$CHECK_DIR/expanded/PackageInfo"
APP="$CHECK_DIR/expanded/Payload/Applications/RuntimeAtlas.app"
MAIN="$APP/Contents/MacOS/RuntimeAtlas"
CLI="$APP/Contents/MacOS/runtime-atlas"
SUPERVISOR="$APP/Contents/MacOS/runtime-atlas-supervisor"
GLOBAL_CLI="$CHECK_DIR/expanded/Payload/usr/local/bin/runtime-atlas"

if [[ ! -d "$APP" || -L "$APP" || ! -f "$APP/Contents/Info.plist" || -L "$APP/Contents/Info.plist" ]]; then
    echo "package app must be a regular non-symlink bundle" >&2
    exit 1
fi
for executable in "$MAIN" "$CLI" "$SUPERVISOR" "$GLOBAL_CLI"; do
    if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
        echo "package executables must be regular non-symlink files" >&2
        exit 1
    fi
done
test "$(xmllint --xpath 'string(/pkg-info/@identifier)' "$PACKAGE_INFO")" = "com.kmg0308.runtimeatlas.pkg"
test "$(xmllint --xpath 'string(/pkg-info/@install-location)' "$PACKAGE_INFO")" = "/"
test "$(xmllint --xpath 'string(/pkg-info/@relocatable)' "$PACKAGE_INFO")" = "false"
cmp -s "$CLI" "$GLOBAL_CLI"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist")" = "com.kmg0308.runtimeatlas"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Contents/Info.plist")" = "RuntimeAtlas"
test "$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$APP/Contents/Info.plist")" = "13.0"
test "$(xmllint --xpath 'string(/pkg-info/@version)' "$PACKAGE_INFO")" = "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")"

MAIN_ARCHS="$(lipo -archs "$MAIN")"
test "$MAIN_ARCHS" = "$(lipo -archs "$CLI")"
test "$MAIN_ARCHS" = "$(lipo -archs "$SUPERVISOR")"
codesign --verify --deep --strict "$APP"
codesign --verify --strict "$CLI"
codesign --verify --strict "$SUPERVISOR"
codesign --verify --strict "$GLOBAL_CLI"

developer_team() {
    local details team
    details="$(codesign -dv --verbose=4 "$1" 2>&1)"
    if ! grep -q '^Authority=Developer ID Application:' <<<"$details"; then
        return 1
    fi
    team="$(sed -n 's/^TeamIdentifier=//p' <<<"$details")"
    if [[ -z "$team" || "$team" = "not set" ]]; then
        return 1
    fi
    printf '%s' "$team"
}

set +e
SIGNATURE_OUTPUT="$(pkgutil --check-signature "$PKG" 2>&1)"
SIGNATURE_STATUS=$?
set -e
case "$SIGNATURE_EXPECTATION" in
    signed)
        if [[ "$SIGNATURE_STATUS" -ne 0 ]] || ! grep -q 'Developer ID Installer:' <<<"$SIGNATURE_OUTPUT"; then
            echo "Developer ID Installer signature required" >&2
            exit 1
        fi
        ;;
    unsigned)
        if [[ "$SIGNATURE_STATUS" -eq 0 ]] || ! grep -q 'Status: no signature' <<<"$SIGNATURE_OUTPUT"; then
            echo "unsigned installer required" >&2
            exit 1
        fi
        ;;
    auto)
        if [[ "$SIGNATURE_STATUS" -ne 0 ]] && ! grep -q 'Status: no signature' <<<"$SIGNATURE_OUTPUT"; then
            echo "installer signature could not be verified" >&2
            exit 1
        fi
        ;;
esac

if [[ "$SIGNATURE_EXPECTATION" = "signed" || "$NOTARIZATION_EXPECTATION" = "stapled" ]]; then
    if [[ "$SIGNATURE_STATUS" -ne 0 ]] || ! grep -q 'Developer ID Installer:' <<<"$SIGNATURE_OUTPUT"; then
        echo "Developer ID Installer signature required" >&2
        exit 1
    fi
    if ! TEAM_IDENTIFIER="$(developer_team "$APP")"; then
        echo "Developer ID Application signature required" >&2
        exit 1
    fi
    for executable in "$MAIN" "$CLI" "$SUPERVISOR" "$GLOBAL_CLI"; do
        if ! EXECUTABLE_TEAM="$(developer_team "$executable")" || [[ "$EXECUTABLE_TEAM" != "$TEAM_IDENTIFIER" ]]; then
            echo "Developer ID Application TeamIdentifier mismatch" >&2
            exit 1
        fi
    done
    grep -Fq "($TEAM_IDENTIFIER)" <<<"$SIGNATURE_OUTPUT"
fi

if [[ "$NOTARIZATION_EXPECTATION" = "stapled" ]]; then
    xcrun stapler validate "$PKG"
    spctl --assess --type execute "$APP"
    spctl --assess --type install "$PKG"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$SCRIPT_DIR/verify-runtime-atlas-macos-executables.sh" "$CLI" "$SUPERVISOR"
"$SCRIPT_DIR/verify-runtime-atlas-macos-executables.sh" "$GLOBAL_CLI" "$SUPERVISOR"

echo "verified $PKG ($MAIN_ARCHS, $SIGNATURE_EXPECTATION installer, $NOTARIZATION_EXPECTATION notarization)"
