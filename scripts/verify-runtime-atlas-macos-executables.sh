#!/usr/bin/env bash
set -euo pipefail

CLI="${1:-}"
SUPERVISOR="${2:-}"
APP_HELPER="${3:-}"
EXECUTABLES=("$CLI" "$SUPERVISOR")
if [[ -n "$APP_HELPER" ]]; then
    EXECUTABLES+=("$APP_HELPER")
fi
for executable in "${EXECUTABLES[@]}"; do
    if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
        echo "RuntimeAtlas helper must be a regular non-symlink executable" >&2
        exit 1
    fi
done
if [[ -n "$APP_HELPER" ]] && ! cmp -s "$CLI" "$APP_HELPER"; then
    echo "RuntimeAtlas app helper must exactly match the bundled CLI" >&2
    exit 1
fi

CHECK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/runtime-atlas-helper-check.XXXXXX")"
trap 'rm -rf "$CHECK_DIR"' EXIT
mkdir -p "$CHECK_DIR/home" "$CHECK_DIR/tmp" "$CHECK_DIR/data"

verify_cli() {
    local executable="$1" output="$2"
    if ! HOME="$CHECK_DIR/home" TMPDIR="$CHECK_DIR/tmp" RUNTIME_ATLAS_HOME="$CHECK_DIR/data" \
        "$executable" help >"$output" 2>"$CHECK_DIR/cli.stderr"; then
        echo "runtime-atlas help contract failed" >&2
        exit 1
    fi
    if ! grep -Fqx 'Runtime Atlas reads local worktree and runtime state.' "$output"; then
        echo "runtime-atlas help contract failed" >&2
        exit 1
    fi
}

verify_cli "$CLI" "$CHECK_DIR/cli.stdout"
if [[ -n "$APP_HELPER" ]]; then
    verify_cli "$APP_HELPER" "$CHECK_DIR/app-helper.stdout"
fi

set +e
HOME="$CHECK_DIR/home" TMPDIR="$CHECK_DIR/tmp" RUNTIME_ATLAS_HOME="$CHECK_DIR/data" \
    "$SUPERVISOR" >"$CHECK_DIR/supervisor.stdout" 2>"$CHECK_DIR/supervisor.stderr"
SUPERVISOR_STATUS=$?
set -e
if [[ "$SUPERVISOR_STATUS" -ne 64 ]] || ! grep -Fq 'usage: runtime-atlas-supervisor ' "$CHECK_DIR/supervisor.stderr"; then
    echo "runtime-atlas-supervisor usage contract failed" >&2
    exit 1
fi
