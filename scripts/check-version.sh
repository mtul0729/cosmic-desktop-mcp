#!/usr/bin/env bash
# The version is written in three places: Cargo.toml, the cosmic-desktop-mcp
# entry in Cargo.lock and the crane package in flake.nix. They drift silently —
# the build still works, only the tag and the binary disagree later.
#
# Usage: scripts/check-version.sh [--tag vX.Y.Z]
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

wanted=""
if [ "${1:-}" = "--tag" ]; then
  wanted="${2:?--tag needs a value}"
  wanted="${wanted#v}"
fi

cargo=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
lock=$(awk '
  /^name = "cosmic-desktop-mcp"$/ { seen = 1; next }
  seen && /^version = "/ { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }
' Cargo.lock)
flake=$(sed -n 's/^ *version = "\(.*\)";$/\1/p' flake.nix | head -1)

status=0
check() {
  local name="$1" value="$2"
  if [ -z "$value" ]; then
    echo "::error::$name has no version"
    status=1
  elif [ -n "$wanted" ] && [ "$value" != "$wanted" ]; then
    echo "::error::$name says $value, the tag says $wanted"
    status=1
  else
    echo "$name: $value"
  fi
}

check Cargo.toml "$cargo"
check Cargo.lock "$lock"
check flake.nix "$flake"

if [ -n "$cargo" ] && { [ "$cargo" != "$lock" ] || [ "$cargo" != "$flake" ]; }; then
  echo "::error::version mismatch: Cargo.toml $cargo, Cargo.lock $lock, flake.nix $flake"
  status=1
fi

exit $status
