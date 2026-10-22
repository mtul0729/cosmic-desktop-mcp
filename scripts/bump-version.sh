#!/usr/bin/env bash
# Write the version into the three files that carry it. Committing and tagging
# is left to the maintainer: this repository is worked on with jj, and a script
# that commits would fight it.
#
# Usage: scripts/bump-version.sh 0.2.1
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

new="${1:?usage: scripts/bump-version.sh <version>}"
new="${new#v}"
if ! printf '%s' "$new" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "not a version: $new" >&2
  exit 1
fi

sed -i "0,/^version = \".*\"$/s//version = \"$new\"/" Cargo.toml
sed -i "s/^ *version = \".*\";$/          version = \"$new\";/" flake.nix
awk -v v="$new" '
  /^name = "cosmic-desktop-mcp"$/ { print; seen = 1; next }
  seen && /^version = "/ { print "version = \"" v "\""; seen = 0; next }
  { print }
' Cargo.lock > Cargo.lock.new && mv Cargo.lock.new Cargo.lock

echo "version written: $new"
scripts/check-version.sh
