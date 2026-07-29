#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.1.0}"; BUILD_NUMBER="${BUILD_NUMBER:-1}"; DIST="$ROOT_DIR/dist"; APP="$DIST/DevConsole.app"; COMPONENT_PLIST="$DIST/DevConsole-component.plist"; ICON="$ROOT_DIR/Resources/DevConsole.icns"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "VERSION must be numeric semver" >&2; exit 2; }
[[ "$BUILD_NUMBER" =~ ^[0-9]+$ ]] || { echo "BUILD_NUMBER must be numeric" >&2; exit 2; }
test -f "$ICON"
cd "$ROOT_DIR"
swift build -c release --product DevConsole -Xswiftc -warnings-as-errors
swift build -c release --product DevConsoleRuntimeAtlasCLI -Xswiftc -warnings-as-errors
swift build -c release --product DevConsoleRuntimeAtlasSupervisor -Xswiftc -warnings-as-errors
BIN="$(swift build -c release --show-bin-path)"
rm -rf "$DIST"; mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources"
install -m 0755 "$BIN/DevConsole" "$APP/Contents/MacOS/DevConsole"
install -m 0755 "$BIN/DevConsoleRuntimeAtlasCLI" "$APP/Contents/Helpers/runtime-atlas"
install -m 0755 "$BIN/DevConsoleRuntimeAtlasSupervisor" "$APP/Contents/Helpers/runtime-atlas-supervisor"
install -m 0644 "$ICON" "$APP/Contents/Resources/DevConsole.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>CFBundleExecutable</key><string>DevConsole</string><key>CFBundleIdentifier</key><string>com.kmg0308.devconsole</string><key>CFBundleName</key><string>DevConsole</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleIconFile</key><string>DevConsole.icns</string><key>CFBundleShortVersionString</key><string>$VERSION</string><key>CFBundleVersion</key><string>$BUILD_NUMBER</string><key>LSMinimumSystemVersion</key><string>13.0</string></dict></plist>
PLIST
codesign --force --sign - "$APP/Contents/Helpers/runtime-atlas" >/dev/null; codesign --force --sign - "$APP/Contents/Helpers/runtime-atlas-supervisor" >/dev/null; codesign --force --deep --sign - "$APP" >/dev/null
codesign --verify --deep --strict "$APP"
for helper in runtime-atlas runtime-atlas-supervisor; do test -x "$APP/Contents/Helpers/$helper"; test ! -L "$APP/Contents/Helpers/$helper"; codesign --verify --strict "$APP/Contents/Helpers/$helper"; done
plutil -lint "$APP/Contents/Info.plist" >/dev/null
ditto -c -k --norsrc --noextattr --noqtn --keepParent "$APP" "$DIST/DevConsole-$VERSION.zip"; cp "$DIST/DevConsole-$VERSION.zip" "$DIST/DevConsole.zip"
mkdir -p "$DIST/pkgroot/Applications"; ditto "$APP" "$DIST/pkgroot/Applications/DevConsole.app"
cat > "$COMPONENT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><dict><key>BundleHasStrictIdentifier</key><true/><key>BundleIsRelocatable</key><false/><key>BundleIsVersionChecked</key><true/><key>BundleOverwriteAction</key><string>upgrade</string><key>RootRelativeBundlePath</key><string>./Applications/DevConsole.app</string></dict></array></plist>
PLIST
pkgbuild --root "$DIST/pkgroot" --component-plist "$COMPONENT_PLIST" --install-location / --identifier com.kmg0308.devconsole.pkg --version "$VERSION" "$DIST/DevConsole-$VERSION.pkg" >/dev/null; cp "$DIST/DevConsole-$VERSION.pkg" "$DIST/DevConsole.pkg"
rm -f "$COMPONENT_PLIST"
python3 - "$DIST/manifest.json" "$VERSION" "$BUILD_NUMBER" <<'PY'
import json, sys
json.dump({"app":"DevConsole.app","version":sys.argv[2],"build":sys.argv[3],"zip":f"DevConsole-{sys.argv[2]}.zip","pkg":f"DevConsole-{sys.argv[2]}.pkg","latestZip":"DevConsole.zip","latestPkg":"DevConsole.pkg"}, open(sys.argv[1], "w"), indent=2)
PY
