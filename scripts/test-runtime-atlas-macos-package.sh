#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_INPUT="${1:-}"
if [[ "$(uname -s)" != "Darwin" || ! -d "$APP_INPUT" ]]; then
    echo "usage: $0 /path/to/RuntimeAtlas.app" >&2
    exit 64
fi

TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/runtime-atlas-package-test.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT

expect_failure() {
    if "$@" >"$TEST_DIR/failure.stdout" 2>"$TEST_DIR/failure.stderr"; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
}

"$ROOT_DIR/scripts/package-runtime-atlas-macos.sh" "$APP_INPUT" "$TEST_DIR/RuntimeAtlas.pkg" >/dev/null
"$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$TEST_DIR/RuntimeAtlas.pkg" unsigned none >/dev/null
expect_failure "$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$TEST_DIR/RuntimeAtlas.pkg" signed none
expect_failure "$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$TEST_DIR/RuntimeAtlas.pkg" auto stapled

expect_failure env \
    APP_SIGN_IDENTITY=- \
    INSTALLER_SIGN_IDENTITY='Developer ID Installer: unavailable' \
    "$ROOT_DIR/scripts/package-runtime-atlas-macos.sh" "$APP_INPUT" "$TEST_DIR/invalid-signed.pkg"
grep -q 'requires a Developer ID Application' "$TEST_DIR/failure.stderr"

for helper in runtime-atlas runtime-atlas-supervisor; do
    FIXTURE="$TEST_DIR/$helper.app"
    ditto "$APP_INPUT" "$FIXTURE"
    mv "$FIXTURE/Contents/MacOS/$helper" "$FIXTURE/Contents/MacOS/$helper.real"
    ln -s "$helper.real" "$FIXTURE/Contents/MacOS/$helper"
    expect_failure "$ROOT_DIR/scripts/package-runtime-atlas-macos.sh" "$FIXTURE" "$TEST_DIR/$helper.pkg"
    grep -q 'regular non-symlink' "$TEST_DIR/failure.stderr"
done

cat > "$TEST_DIR/component.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><dict>
  <key>BundleHasStrictIdentifier</key><true/>
  <key>BundleIsRelocatable</key><false/>
  <key>RootRelativeBundlePath</key><string>Applications/RuntimeAtlas.app</string>
</dict></array></plist>
PLIST
for helper in runtime-atlas runtime-atlas-supervisor; do
    EXPANDED="$TEST_DIR/$helper-expanded"
    pkgutil --expand-full "$TEST_DIR/RuntimeAtlas.pkg" "$EXPANDED" >/dev/null
    PAYLOAD_HELPER="$EXPANDED/Payload/Applications/RuntimeAtlas.app/Contents/MacOS/$helper"
    mv "$PAYLOAD_HELPER" "$PAYLOAD_HELPER.real"
    ln -s "$helper.real" "$PAYLOAD_HELPER"
    pkgbuild \
        --root "$EXPANDED/Payload" \
        --component-plist "$TEST_DIR/component.plist" \
        --install-location / \
        --identifier com.kmg0308.runtimeatlas.pkg \
        --version "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$EXPANDED/Payload/Applications/RuntimeAtlas.app/Contents/Info.plist")" \
        "$TEST_DIR/$helper-symlink.pkg" >/dev/null
    expect_failure "$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$TEST_DIR/$helper-symlink.pkg" unsigned none
    grep -q 'regular non-symlink' "$TEST_DIR/failure.stderr"
done

pkgutil --expand-full "$TEST_DIR/RuntimeAtlas.pkg" "$TEST_DIR/swap-expanded" >/dev/null
SWAP_PAYLOAD="$TEST_DIR/swap-expanded/Payload"
SWAP_APP="$SWAP_PAYLOAD/Applications/RuntimeAtlas.app"
install -m 0755 "$SWAP_APP/Contents/MacOS/runtime-atlas-supervisor" "$SWAP_APP/Contents/MacOS/runtime-atlas"
codesign --force --sign - "$SWAP_APP/Contents/MacOS/runtime-atlas"
install -m 0755 "$SWAP_APP/Contents/MacOS/runtime-atlas" "$SWAP_PAYLOAD/usr/local/bin/runtime-atlas"
codesign --force --sign - "$SWAP_APP"
pkgbuild \
    --root "$SWAP_PAYLOAD" \
    --component-plist "$TEST_DIR/component.plist" \
    --install-location / \
    --identifier com.kmg0308.runtimeatlas.pkg \
    --version "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$SWAP_APP/Contents/Info.plist")" \
    "$TEST_DIR/swapped.pkg" >/dev/null
test "$(lipo -archs "$SWAP_APP/Contents/MacOS/RuntimeAtlas")" = "$(lipo -archs "$SWAP_APP/Contents/MacOS/runtime-atlas")"
codesign --verify --deep --strict "$SWAP_APP"
codesign --verify --strict "$SWAP_PAYLOAD/usr/local/bin/runtime-atlas"
expect_failure "$ROOT_DIR/scripts/verify-runtime-atlas-macos-package.sh" "$TEST_DIR/swapped.pkg" unsigned none
grep -q 'runtime-atlas help contract failed' "$TEST_DIR/failure.stderr"

echo "RuntimeAtlas macOS package tests passed"
