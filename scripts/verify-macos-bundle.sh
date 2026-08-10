#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

prepare_token_meter_fixture() {
    local home="$1" identifier="$2" settings
    case "$identifier" in
        local.tokenmeter.app|com.kmg0308.devconsole)
            settings="$home/Library/Application Support/TokenMeter/settings.json"
            mkdir -p "$(dirname "$settings")"
            umask 077
            cat >"$settings" <<'JSON'
{"schemaVersion":3,"showFullTokenNumbers":false,"localDeviceId":"macos-bundle-verifier","codexExecutablePath":"/usr/bin/false"}
JSON
            ;;
    esac
}

if [[ "${1:-}" == --self-test ]]; then
    SELF_TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dev-console-macos-bundle-self-test.XXXXXX")"
    trap 'rm -rf "$SELF_TEST_DIR"' EXIT
    mkdir -p "$SELF_TEST_DIR/token" "$SELF_TEST_DIR/dev-console" "$SELF_TEST_DIR/runtime"
    prepare_token_meter_fixture "$SELF_TEST_DIR/token" local.tokenmeter.app
    prepare_token_meter_fixture "$SELF_TEST_DIR/dev-console" com.kmg0308.devconsole
    prepare_token_meter_fixture "$SELF_TEST_DIR/runtime" com.kmg0308.runtimeatlas
    SETTINGS="$SELF_TEST_DIR/token/Library/Application Support/TokenMeter/settings.json"
    test "$(/usr/bin/plutil -extract schemaVersion raw -o - "$SETTINGS")" = 3
    test "$(/usr/bin/plutil -extract showFullTokenNumbers raw -o - "$SETTINGS")" = false
    test "$(/usr/bin/plutil -extract localDeviceId raw -o - "$SETTINGS")" = macos-bundle-verifier
    test "$(/usr/bin/plutil -extract codexExecutablePath raw -o - "$SETTINGS")" = /usr/bin/false
    cmp -s "$SETTINGS" "$SELF_TEST_DIR/dev-console/Library/Application Support/TokenMeter/settings.json"
    test ! -e "$SELF_TEST_DIR/runtime/Library/Application Support/TokenMeter"
    echo "macOS bundle verifier self-test passed"
    exit 0
fi

DMG="${1:-}"
PRODUCT="${2:-}"
BINARY="${3:-}"
IDENTIFIER="${4:-}"
VERSION="${5:-}"
RUNTIME_FEATURE="${6:-}"
if [[ "$(uname -s)" != "Darwin" || ! -f "$DMG" || -L "$DMG" || -z "$PRODUCT" || -z "$BINARY" || -z "$IDENTIFIER" || -z "$VERSION" || ! "$RUNTIME_FEATURE" =~ ^(true|false)$ ]]; then
    echo "usage: $0 App.dmg ProductName MainBinaryName BundleIdentifier Version true|false" >&2
    exit 64
fi

CHECK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/dev-console-macos-bundle.XXXXXX")"
MOUNT="$CHECK_DIR/mount"
INSTALLED_APP="$CHECK_DIR/Applications/$PRODUCT.app"
PID=""
MOUNTED=0
cleanup() {
    local status=$?
    trap - EXIT
    if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    if [[ "$MOUNTED" == 1 ]] && ! hdiutil detach "$MOUNT" >/dev/null; then
        status=1
    fi
    rm -rf "$CHECK_DIR"
    exit "$status"
}
trap cleanup EXIT

mkdir -p "$MOUNT" "$(dirname "$INSTALLED_APP")" "$CHECK_DIR/home" "$CHECK_DIR/tmp"
hdiutil attach -readonly -nobrowse -mountpoint "$MOUNT" "$DMG" >/dev/null
MOUNTED=1
MOUNTED_APP="$MOUNT/$PRODUCT.app"
if [[ ! -d "$MOUNTED_APP" || -L "$MOUNTED_APP" ]]; then
    echo "DMG must contain a regular $PRODUCT.app bundle" >&2
    exit 1
fi
ditto "$MOUNTED_APP" "$INSTALLED_APP"

INFO="$INSTALLED_APP/Contents/Info.plist"
MAIN="$INSTALLED_APP/Contents/MacOS/$BINARY"
if [[ ! -f "$INFO" || -L "$INFO" || ! -f "$MAIN" || -L "$MAIN" || ! -x "$MAIN" ]]; then
    echo "installed app metadata and main executable must be regular files" >&2
    exit 1
fi
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$INFO")" = "$IDENTIFIER"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$INFO")" = "$VERSION"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$INFO")" = "$BINARY"
test "$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$INFO")" = "13.0"

verify_universal() {
    local executable="$1" archs
    archs="$(lipo -archs "$executable")"
    if [[ " $archs " != *" arm64 "* || " $archs " != *" x86_64 "* || "$(wc -w <<<"$archs" | tr -d ' ')" != 2 ]]; then
        echo "installed executable must contain exactly arm64 and x86_64: $executable ($archs)" >&2
        exit 1
    fi
}

ARCHS="$(lipo -archs "$MAIN")"
verify_universal "$MAIN"

if [[ "$RUNTIME_FEATURE" == true ]]; then
    HELPER_ARGS=(
        "$INSTALLED_APP/Contents/MacOS/runtime-atlas"
        "$INSTALLED_APP/Contents/MacOS/runtime-atlas-supervisor"
    )
    if [[ "$IDENTIFIER" == com.kmg0308.runtimeatlas ]]; then
        HELPER_ARGS+=("$INSTALLED_APP/Contents/Helpers/runtime-atlas")
    fi
    for executable in "${HELPER_ARGS[@]}"; do
        verify_universal "$executable"
    done
    "$ROOT_DIR/scripts/verify-runtime-atlas-macos-executables.sh" "${HELPER_ARGS[@]}"
fi

prepare_token_meter_fixture "$CHECK_DIR/home" "$IDENTIFIER"
env -u CODEX_HOME HOME="$CHECK_DIR/home" TMPDIR="$CHECK_DIR/tmp" "$MAIN" \
    >"$CHECK_DIR/main.stdout" 2>"$CHECK_DIR/main.stderr" &
PID=$!
sleep 3
if ! kill -0 "$PID" 2>/dev/null; then
    wait "$PID" || true
    PID=""
    echo "$PRODUCT exited during the startup smoke test" >&2
    sed -n '1,40p' "$CHECK_DIR/main.stderr" >&2
    exit 1
fi
kill "$PID"
wait "$PID" 2>/dev/null || true
PID=""

echo "verified $PRODUCT DMG install and startup ($ARCHS, macOS 13.0+)"
