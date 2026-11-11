#!/usr/bin/env bash
# Applies a release version to this checkout (CI only; nothing is committed):
#  - the Cargo workspace version, which `drive` and `drive-app` report, and
#  - app/release.conf.json, the Tauri config override passed to `tauri build --config`
#    (bundle version, plus the signed updater artifacts the release publishes).
#
#   .github/release/set-version.sh 0.1.42
set -euo pipefail

version="${1:?usage: set-version.sh <version>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

sed -i.bak '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "'"$version"'"/' \
  "$root/Cargo.toml"
rm -f "$root/Cargo.toml.bak"
grep -q "^version = \"$version\"" "$root/Cargo.toml" || {
  echo "could not set the workspace version in Cargo.toml" >&2
  exit 1
}

cat > "$root/app/release.conf.json" <<JSON
{
  "version": "$version",
  "bundle": { "createUpdaterArtifacts": true }
}
JSON
echo "version $version applied"
