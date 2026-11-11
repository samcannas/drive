#!/usr/bin/env bash
# Builds the `drive` service and copies it to app/src-tauri/binaries/drive-<target-triple>,
# where Tauri's `externalBin` expects it. Run from anywhere.
#
#   scripts/prepare-sidecar.sh            # release build for the host
#   scripts/prepare-sidecar.sh --debug    # debug build (faster, for local development)
#   scripts/prepare-sidecar.sh --target x86_64-unknown-linux-gnu
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile=release
target=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --debug) profile=debug ;;
    --target) target="$2"; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

triple="${target:-$(rustc -vV | sed -n 's/^host: //p')}"
args=(build -p drive-daemon --bin drive)
[[ "$profile" == release ]] && args+=(--release)
[[ -n "$target" ]] && args+=(--target "$target")
(cd "$root" && cargo "${args[@]}")

target_dir="$(cd "$root" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
src="$target_dir/${target:+$target/}$profile/drive"
ext=""
[[ "$triple" == *windows* ]] && ext=".exe"
mkdir -p "$root/app/src-tauri/binaries"
cp "$src$ext" "$root/app/src-tauri/binaries/drive-$triple$ext"
echo "sidecar ready: app/src-tauri/binaries/drive-$triple$ext"
