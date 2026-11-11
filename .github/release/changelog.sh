#!/usr/bin/env bash
# Builds <dist>/changelog.json for release v<version> from the git history: one entry per
# release (this one plus earlier `v*` tags, newest first, at most 30), each with the commit
# subjects since the previous release turned into short, user-facing notes.
#
# The app's updater shows these notes (everything newer than the installed version), and
# make-release-files.sh reuses this release's notes for latest.json and the release page.
#
#   .github/release/changelog.sh 0.1.42 dist        (needs full history: fetch-depth: 0)
set -euo pipefail

version="${1:?usage: changelog.sh <version> <dist>}"
dist="${2:?usage: changelog.sh <version> <dist>}"
repo="${GITHUB_REPOSITORY:-samcannas/drive}"
mkdir -p "$dist"

# Commit subjects in <range>, as notes: internal housekeeping is dropped, "Area: " prefixes
# removed, "a; b" split into separate notes, first letter capitalised, duplicates removed.
notes_for() {
  git log --no-merges --format=%s "$1" \
    | { grep -Ev '^(CI|Release|Docs|Format|Formatting|Merge|Windows tests|Tests?)\b' || true; } \
    | sed -E 's/^[A-Za-z][A-Za-z /-]{0,24}: //' \
    | sed 's/; /\n/g' \
    | sed -E 's/^[[:space:]]+|[[:space:]]+$//g; s/\.$//' \
    | awk 'length > 0 { print toupper(substr($0,1,1)) substr($0,2) }' \
    | awk '!seen[tolower($0)]++' \
    | head -n 20 \
    | jq -R . | jq -s .
}

entry() { # version, date, notes-json
  jq -n --arg v "$1" --arg d "$2" --arg url "https://github.com/$repo/releases/tag/v$1" --argjson n "$3" \
    '{version: $v, date: $d, url: $url, notes: (if ($n | length) == 0 then ["Reliability improvements and fixes"] else $n end)}'
}

mapfile -t tags < <(git tag -l 'v[0-9]*' --sort=-v:refname | grep -vx "v$version" || true)

entries=()
# This release: everything since the newest earlier release.
if [ "${#tags[@]}" -gt 0 ]; then
  current_notes="$(notes_for "${tags[0]}..HEAD")"
else
  current_notes='["First release"]'
fi
entries+=("$(entry "$version" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$current_notes")")

# Earlier releases, each against the one before it.
for i in "${!tags[@]}"; do
  [ "${#entries[@]}" -ge 30 ] && break
  tag="${tags[$i]}"
  date="$(git log -1 --format=%cI "$tag")"
  if [ $((i + 1)) -lt "${#tags[@]}" ]; then
    n="$(notes_for "${tags[$((i + 1))]}..$tag")"
  else
    n='["First release"]'
  fi
  entries+=("$(entry "${tag#v}" "$date" "$n")")
done

printf '%s\n' "${entries[@]}" | jq -s '{releases: .}' > "$dist/changelog.json"
jq -r '.releases[0].notes[] | "- " + .' "$dist/changelog.json" > whatsnew.md
cat "$dist/changelog.json"
