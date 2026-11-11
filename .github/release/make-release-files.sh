#!/usr/bin/env bash
# Writes the updater manifest (<dist>/latest.json), <dist>/SHA256SUMS and ./notes.md for
# release v<version> from the bundles in <dist>.
#
#   .github/release/make-release-files.sh 0.1.42 dist
set -euo pipefail

version="${1:?usage: make-release-files.sh <version> <dist>}"
dist="${2:?usage: make-release-files.sh <version> <dist>}"
repo="${GITHUB_REPOSITORY:-samcannas/drive}"
base="https://github.com/$repo/releases/download/v$version"

exe="Drive_${version}_x64-setup.exe"
deb="Drive_${version}_amd64.deb"
appimage="Drive_${version}_amd64.AppImage"
pkg="drive-${version}-1-x86_64.pkg.tar.zst"
for f in "$exe" "$exe.sig" "$deb" "$appimage" "$appimage.sig" "$pkg"; do
  test -f "$dist/$f" || { echo "missing $dist/$f" >&2; exit 1; }
done

# Tauri updater manifest. The plugin looks up "<os>-<arch>-<bundle>" first, then "<os>-<arch>".
entry() { jq -n --arg url "$base/$1" --rawfile sig "$dist/$1.sig" '{url: $url, signature: $sig}'; }
platforms="$(jq -n \
  --argjson win "$(entry "$exe")" \
  --argjson appimage "$(entry "$appimage")" \
  '{"windows-x86_64": $win, "windows-x86_64-nsis": $win,
    "linux-x86_64": $appimage, "linux-x86_64-appimage": $appimage}')"
if [ -f "$dist/$deb.sig" ]; then
  platforms="$(jq --argjson deb "$(entry "$deb")" '. + {"linux-x86_64-deb": $deb}' <<<"$platforms")"
fi
jq -n \
  --arg version "$version" \
  --arg notes "Drive $version: https://github.com/$repo/releases/tag/v$version" \
  --arg date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --argjson platforms "$platforms" \
  '{version: $version, notes: $notes, pub_date: $date, platforms: $platforms}' \
  > "$dist/latest.json"

(cd "$dist" && sha256sum -- $(ls | grep -vx SHA256SUMS) > SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS)

self_updating="the Windows installer and the AppImage"
if [ -f "$dist/$deb.sig" ]; then
  self_updating="the Windows installer, the AppImage and the .deb (it asks for your password)"
fi
commit="${GITHUB_SHA:-$(git rev-parse HEAD)}"
cat > notes.md <<NOTES
Drive $version, built from ${commit:0:12}.

The repository is private, so download with the GitHub CLI (or from this page while signed in):

\`\`\`sh
gh release download v$version --repo $repo
sha256sum --check --ignore-missing SHA256SUMS
\`\`\`

### Windows 10/11 (x64)
Run **$exe**. It installs for your user only (no administrator rights needed) and starts Drive.
The installer is not code-signed yet, so SmartScreen may warn: choose *More info → Run anyway*.

### Ubuntu / Debian (x64)
\`\`\`sh
sudo apt install ./$deb
systemctl --user enable --now drive     # the sync service, as your user (not root)
\`\`\`
Then open **Drive** from the application menu. Needs Ubuntu 22.04+ / Debian 12+ (WebKitGTK 4.1);
installs \`fuse3\` as a dependency.

### Arch Linux / Manjaro / Omarchy
\`\`\`sh
sudo pacman -U ./$pkg
systemctl --user enable --now drive
\`\`\`

### Other Linux (AppImage)
\`\`\`sh
chmod +x $appimage
./$appimage
\`\`\`
Mounting the Drive folder needs FUSE 3 (\`fuse3\` package). If the AppImage itself doesn't start,
install FUSE 2 as well (\`libfuse2t64\` on Ubuntu 24.04, \`fuse2\` on Arch).

Updates: $self_updating update themselves (tray menu: *Restart to update*); for other packages,
install the newer package the same way.
NOTES
cat "$dist/latest.json"
cat "$dist/SHA256SUMS"
