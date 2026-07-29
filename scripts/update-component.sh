#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPONENT="${1:?component required}"
REVISION="${2:?40-character revision required}"
VERSION="${3:?version required}"
TAG="${4:?tag required}"
case "$COMPONENT" in runtime-atlas|token-meter) ;; *) echo "unsupported component" >&2; exit 2;; esac
[[ "$REVISION" =~ ^[0-9a-f]{40}$ ]] || { echo "revision must be 40 lowercase hex characters" >&2; exit 2; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || { echo "version must be numeric semver" >&2; exit 2; }
[[ "$TAG" == "v$VERSION" ]] || { echo "tag must equal vVERSION" >&2; exit 2; }
PACKAGE="${PACKAGE_FILE:-$ROOT_DIR/Package.swift}"
[[ -f "$PACKAGE" ]] || { echo "Package.swift not found" >&2; exit 2; }
case "$COMPONENT" in
  runtime-atlas) URL='https://github.com/kmg0308/runtime_atlas.git' ;;
  token-meter) URL='https://github.com/kmg0308/token-scope.git' ;;
esac
TAG_REFS="$(git ls-remote --exit-code "$URL" "refs/tags/$TAG" "refs/tags/$TAG^{}")" || {
  echo "component release tag does not exist" >&2
  exit 2
}
TAG_REVISION="$(
  printf '%s\n' "$TAG_REFS" | awk -v tag="$TAG" '
    $2 == "refs/tags/" tag { direct = $1 }
    $2 == "refs/tags/" tag "^{}" { peeled = $1 }
    END { print (peeled != "" ? peeled : direct) }
  '
)"
[[ "$TAG_REVISION" == "$REVISION" ]] || {
  echo "component release tag does not resolve to revision" >&2
  exit 2
}
python3 - "$PACKAGE" "$URL" "$REVISION" <<'PY'
import os, re, stat, sys, tempfile
path, url, revision = sys.argv[1:]
source = open(path).read()
pattern = r'(\.package\(\s*url:\s*"' + re.escape(url) + r'",\s*revision:\s*")[0-9a-f]{40}("\s*\))'
updated, count = re.subn(pattern, r'\g<1>' + revision + r'\g<2>', source)
if count != 1:
    raise SystemExit("Package.swift must contain exactly one component revision pin")
mode = stat.S_IMODE(os.stat(path).st_mode)
with tempfile.NamedTemporaryFile("w", dir=os.path.dirname(path), delete=False) as output:
    output.write(updated)
    temporary = output.name
os.chmod(temporary, mode)
os.replace(temporary, path)
PY
if [[ "$PACKAGE" == "$ROOT_DIR/Package.swift" ]]; then swift package resolve; fi
