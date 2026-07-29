#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-0.1.0}"
BUILD_NUMBER="${BUILD_NUMBER:-1}"
cd "$ROOT_DIR"

bash -n scripts/package.sh scripts/update-component.sh scripts/verify.sh
if VERSION=invalid scripts/package.sh >/dev/null 2>&1; then
  echo "package script should reject an invalid version" >&2
  exit 1
fi
swift run DevConsoleSelfTest

python3 - <<'PY'
import json
import pathlib
import re

root = pathlib.Path(".")
manifest = (root / "Package.swift").read_text()
resolved = json.loads((root / "Package.resolved").read_text())
expected = {
    "runtime_atlas": (
        "https://github.com/kmg0308/runtime_atlas.git",
        r"https://github\.com/kmg0308/runtime_atlas\.git",
    ),
    "token-scope": (
        "https://github.com/kmg0308/token-scope.git",
        r"https://github\.com/kmg0308/token-scope\.git",
    ),
}
pins = {pin["identity"]: pin for pin in resolved["pins"]}
assert set(pins) == set(expected), "Package.resolved must contain only the two component pins"
for identity, (url, escaped_url) in expected.items():
    revisions = re.findall(
        r'\.package\(\s*url:\s*"' + escaped_url
        + r'",\s*revision:\s*"([0-9a-f]{40})"\s*\)',
        manifest,
    )
    assert len(revisions) == 1, f"{identity} must have exactly one revision pin"
    pin = pins[identity]
    assert pin["location"] == url
    assert pin["state"]["revision"] == revisions[0]
PY

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin"
cat > "$TMP/bin/git" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == "ls-remote" ]]
printf '%s\trefs/tags/%s\n' "$MOCK_TAG_REVISION" "$MOCK_TAG"
SH
chmod +x "$TMP/bin/git"
cat > "$TMP/Package.swift" <<'SWIFT'
let dependencies = [
  .package(
    url: "https://github.com/kmg0308/runtime_atlas.git",
    revision: "1111111111111111111111111111111111111111"
  ),
  .package(
    url: "https://github.com/kmg0308/token-scope.git",
    revision: "2222222222222222222222222222222222222222"
  )
]
SWIFT
PATH="$TMP/bin:$PATH" \
  MOCK_TAG_REVISION=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef \
  MOCK_TAG=v1.2.3 \
  PACKAGE_FILE="$TMP/Package.swift" \
  scripts/update-component.sh runtime-atlas deadbeefdeadbeefdeadbeefdeadbeefdeadbeef 1.2.3 v1.2.3
grep -Fq 'revision: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"' "$TMP/Package.swift"
grep -Fq 'revision: "2222222222222222222222222222222222222222"' "$TMP/Package.swift"
if PATH="$TMP/bin:$PATH" \
  MOCK_TAG_REVISION=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
  MOCK_TAG=v1.2.3 \
  PACKAGE_FILE="$TMP/Package.swift" \
  scripts/update-component.sh token-meter bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 1.2.3 v1.2.3; then
    echo "fixture should reject a tag that resolves to another revision" >&2
    exit 1
fi
if PACKAGE_FILE="$TMP/Package.swift" scripts/update-component.sh token-meter bad 1.2.3 v1.2.3; then
    echo "fixture should reject malformed revision" >&2
    exit 1
fi

VERSION="$VERSION" BUILD_NUMBER="$BUILD_NUMBER" scripts/package.sh
APP="$ROOT_DIR/dist/DevConsole.app"
test -d "$APP"
test -f "$ROOT_DIR/dist/DevConsole-$VERSION.zip"
test -f "$ROOT_DIR/dist/DevConsole-$VERSION.pkg"
test -f "$ROOT_DIR/dist/DevConsole.zip"
test -f "$ROOT_DIR/dist/DevConsole.pkg"
test -f "$ROOT_DIR/dist/manifest.json"
python3 -m json.tool "$ROOT_DIR/dist/manifest.json" >/dev/null
swift run DevConsoleSelfTest --validate-update-archive "$ROOT_DIR/dist/DevConsole.zip"

codesign --verify --deep --strict "$APP"
plutil -lint "$APP/Contents/Info.plist" >/dev/null
/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Contents/Info.plist" | grep -Fxq 'com.kmg0308.devconsole'
/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$APP/Contents/Info.plist" | grep -Fxq 'DevConsole'
/usr/libexec/PlistBuddy -c 'Print :CFBundleName' "$APP/Contents/Info.plist" | grep -Fxq 'DevConsole'
/usr/libexec/PlistBuddy -c 'Print :CFBundlePackageType' "$APP/Contents/Info.plist" | grep -Fxq 'APPL'
if /usr/libexec/PlistBuddy -c 'Print :LSUIElement' "$APP/Contents/Info.plist" >/dev/null 2>&1; then
  echo "DevConsole must be a normal windowed app" >&2
  exit 1
fi
for helper in runtime-atlas runtime-atlas-supervisor; do
  test -x "$APP/Contents/Helpers/$helper"
  test ! -L "$APP/Contents/Helpers/$helper"
  codesign --verify --strict "$APP/Contents/Helpers/$helper"
done
"$APP/Contents/Helpers/runtime-atlas" help | grep -Fq 'Runtime Atlas reads local worktree and runtime state.'
set +e
"$APP/Contents/Helpers/runtime-atlas-supervisor" >/dev/null 2>&1
SUPERVISOR_STATUS=$?
set -e
test "$SUPERVISOR_STATUS" -eq 64

pkgutil --expand-full "$ROOT_DIR/dist/DevConsole.pkg" "$TMP/pkg-expanded" >/dev/null
grep -Fq 'install-location="/"' "$TMP/pkg-expanded/PackageInfo"
grep -Fq 'relocatable="false"' "$TMP/pkg-expanded/PackageInfo"
if grep -Fq '<relocate>' "$TMP/pkg-expanded/PackageInfo"; then
  echo "DevConsole.pkg must not relocate an existing bundle" >&2
  exit 1
fi

grep -Fq 'RuntimeAtlasFeatureView(' Sources/DevConsoleApp/DevConsoleApp.swift
grep -Fq 'TokenMeterFeatureView(host: tokenHost)' Sources/DevConsoleApp/DevConsoleApp.swift
grep -Fq 'isEnabled: selectedTab == .runtimeAtlas' Sources/DevConsoleApp/DevConsoleApp.swift
grep -Fq 'com.kmg0308.devconsole' Sources/DevConsoleCore/DevConsoleCore.swift
grep -Fq '</dev/null >/dev/null 2>&1 &' Sources/DevConsoleCore/DevConsoleCore.swift
grep -Fq 'NSApplication.shared.terminate(nil)' Sources/DevConsoleApp/UpdateService.swift
grep -Fq 'DevConsoleInstallerScript.requiresAdministrator' Sources/DevConsoleApp/UpdateService.swift
grep -Fq 'with administrator privileges' Sources/DevConsoleApp/UpdateService.swift
test ! -d Sources/RuntimeAtlasFeature
test ! -d Sources/TokenMeterFeature

for f in .github/workflows/*.yml; do ruby -e 'require "yaml"; YAML.load_file(ARGV.fetch(0))' "$f"; done
grep -Fq 'types: [component-released]' .github/workflows/component-update.yml
grep -Fq 'runs-on: macos-26' .github/workflows/component-update.yml
grep -Fq 'group: dev-console-component-update' .github/workflows/component-update.yml
grep -Fq 'persist-credentials: false' .github/workflows/component-update.yml
grep -Fq 'ref: main' .github/workflows/component-update.yml
grep -Fq 'gh auth setup-git' .github/workflows/component-update.yml
grep -Fq 'git add Package.swift Package.resolved' .github/workflows/component-update.yml
grep -Fq 'gh pr merge "$PR_NUMBER" --auto --squash' .github/workflows/component-update.yml
grep -Fq 'DEV_CONSOLE_AUTOMATION_TOKEN' .github/workflows/component-update.yml
grep -Fq 'branches: [main]' .github/workflows/release.yml
! grep -Fq 'workflow_dispatch:' .github/workflows/release.yml
grep -Fq 'test "$(git rev-parse origin/main)" = "$GITHUB_SHA"' .github/workflows/release.yml
grep -Fq 'dist/DevConsole.zip' .github/workflows/release.yml
grep -Fq 'dist/DevConsole.pkg' .github/workflows/release.yml
grep -Fq 'https://github.com/kmg0308/dev-console/releases/latest/download/DevConsole.pkg' README.md

echo "verify passed"
