#!/usr/bin/env bash
# Release build: the app and the `drive` service in one cargo run, then the bundles.
#
#   .github/release/build-app.sh nsis
#   .github/release/build-app.sh deb,appimage
#
# One cargo invocation builds both binaries, so drive-core and every shared dependency
# compile once and the two final links run in parallel. (Building the service first and then
# `tauri build` compiled drive-core twice, with different feature sets, one after the other.)
# The bundle step needs TAURI_SIGNING_PRIVATE_KEY(_PASSWORD) for the updater signatures.
set -euo pipefail

bundles="${1:?usage: build-app.sh <bundles>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root/app"

pnpm install --frozen-lockfile

# No external binaries while compiling: tauri-build would copy binaries/drive-<triple> over
# target/release/drive while cargo is writing that same file. The bundle step adds it.
pnpm tauri build --ci --no-bundle \
  --config release.conf.json \
  --config '{"bundle":{"externalBin":[]}}' \
  -- -p drive-daemon -p drive-app --timings

triple="$(rustc -vV | sed -n 's/^host: //p')"
ext=""
[[ "$triple" == *windows* ]] && ext=".exe"
mkdir -p src-tauri/binaries
cp "$root/target/release/drive$ext" "src-tauri/binaries/drive-$triple$ext"

pnpm tauri bundle --ci --bundles "$bundles" --config release.conf.json
