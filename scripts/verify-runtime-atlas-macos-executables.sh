#!/usr/bin/env bash
set -euo pipefail

CLI="${1:-}"
SUPERVISOR="${2:-}"
for executable in "$CLI" "$SUPERVISOR"; do
    if [[ ! -f "$executable" || -L "$executable" || ! -x "$executable" ]]; then
        echo "RuntimeAtlas helper must be a regular non-symlink executable" >&2
        exit 1
    fi
done

CHECK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/runtime-atlas-helper-check.XXXXXX")"
trap 'rm -rf "$CHECK_DIR"' EXIT
mkdir -p "$CHECK_DIR/home" "$CHECK_DIR/tmp" "$CHECK_DIR/data"

if ! HOME="$CHECK_DIR/home" TMPDIR="$CHECK_DIR/tmp" RUNTIME_ATLAS_HOME="$CHECK_DIR/data" \
    "$CLI" help >"$CHECK_DIR/cli.stdout" 2>"$CHECK_DIR/cli.stderr"; then
    echo "runtime-atlas help contract failed" >&2
    exit 1
fi
if ! grep -Fqx 'Runtime Atlas reads local worktree and runtime state.' "$CHECK_DIR/cli.stdout"; then
    echo "runtime-atlas help contract failed" >&2
    exit 1
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
