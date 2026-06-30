#!/usr/bin/env bash
# Bump version, commit, tag i push. El tag dispara .github/workflows/release.yml
# que construeix l'executable + el container i en fa neteja (manté 5).
#
# Ús:  scripts/release.sh [patch|minor|major]   (per defecte: patch)
set -euo pipefail
cd "$(dirname "$0")/.."

BUMP="${1:-patch}"
case "$BUMP" in patch|minor|major) ;; *) echo "ús: $0 [patch|minor|major]" >&2; exit 1 ;; esac

# Treball net obligatori: el tag ha de reflectir un estat reproduïble.
if [ -n "$(git status --porcelain)" ]; then
  echo "error: working tree brut. Fes commit o stash abans." >&2
  exit 1
fi

CUR=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/version = "([^"]+)"/\1/')
IFS='.' read -r MA MI PA <<<"$CUR"
case "$BUMP" in
  major) MA=$((MA+1)); MI=0; PA=0 ;;
  minor) MI=$((MI+1)); PA=0 ;;
  patch) PA=$((PA+1)) ;;
esac
NEW="$MA.$MI.$PA"
echo "versió: $CUR -> $NEW"

# Cargo.toml: només la línia version del [package] (CUR és únic, les deps usen "1" etc).
sed -i -E "0,/^version = \"$CUR\"/s//version = \"$NEW\"/" Cargo.toml
# Cargo.lock: només dins del bloc del paquet linkanalyzer.
awk -v new="$NEW" '
  /^name = "linkanalyzer"$/ {inpkg=1}
  inpkg && /^version = / {sub(/"[^"]+"/, "\"" new "\""); inpkg=0}
  {print}
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

git add Cargo.toml Cargo.lock
git commit -m "chore: release v$NEW"
git tag -a "v$NEW" -m "v$NEW"
git push origin HEAD "v$NEW"
echo "fet. Tag v$NEW pujat -> el workflow construeix release + container."
