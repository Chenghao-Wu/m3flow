#!/usr/bin/env bash
# m3flow release driver — every release cuts a tag.
#
#   scripts/release.sh <new-version>     e.g.: scripts/release.sh 0.4.1
#
# Does: bump package versions (Cargo workspace + providers pyproject +
# conda recipe) -> sync Cargo.lock -> commit -> tag v<new> -> push ->
# refresh the recipe's tag-tarball sha256 -> push.
#
# CI (release.yml) does the rest on the tag push: build binaries for all
# targets, create the GitHub Release, and publish m3flow-providers to
# PyPI via trusted publishing (no manual twine, no stored token).
#
# NOT touched here: per-provider PROVIDER_VERSION strings. Those are
# cache-invalidation keys — bump one in the same commit as the behavior
# change it belongs to, scoped to the changed provider only.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

NEW="${1:?usage: scripts/release.sh <new-version>   e.g. 0.4.1}"
TAG="v$NEW"
REPO="Chenghao-Wu/m3flow"

# bump-first: nothing uncommitted, on main, tag unused
[ -z "$(git status --porcelain)" ] || { echo "error: working tree not clean"; exit 1; }
[ "$(git branch --show-current)" = "main" ] || { echo "error: not on main"; exit 1; }
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: $TAG already exists"; exit 1
fi

OLD=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
echo "bumping package versions $OLD -> $NEW"
sed -i "s/^version = \"$OLD\"$/version = \"$NEW\"/" Cargo.toml
sed -i "s/^version = \"$OLD\"$/version = \"$NEW\"/" providers/pyproject.toml
sed -i "s/^  version: \"$OLD\"$/  version: \"$NEW\"/" conda/recipe.yaml
export PATH="$HOME/.cargo/bin:$PATH"
cargo check --workspace --quiet   # syncs Cargo.lock's five m3flow crates

git add Cargo.toml Cargo.lock providers/pyproject.toml conda/recipe.yaml
git commit -m "Release $NEW: package-level version bump

Per-provider PROVIDER_VERSIONs unchanged — they bump with the behavior
change they belong to, not with the release train."
git tag -a "$TAG" -m "m3flow $NEW"
git push origin main "$TAG"

# sha256 two-step: the tag tarball exists only after the push. Auth header
# keeps this working while the repo is private; harmless once public.
echo "waiting for $TAG tarball ..."
url="https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz"
code=""
for _ in $(seq 1 24); do
  code=$(curl -sL -H "Authorization: token $(gh auth token)" \
              -o "/tmp/m3flow-$TAG.tar.gz" -w '%{http_code}' "$url" || true)
  [ "$code" = 200 ] && break
  sleep 5
done
[ "$code" = 200 ] || { echo "error: tarball fetch failed (HTTP $code)"; exit 1; }
SHA=$(sha256sum "/tmp/m3flow-$TAG.tar.gz" | cut -d' ' -f1)
sed -i "s/^  sha256: .*/  sha256: $SHA/" conda/recipe.yaml
git add conda/recipe.yaml
git commit -m "Conda recipe: sha256 for the $TAG tag tarball"
git push origin main

echo
echo "released $TAG — CI now builds binaries, creates the GitHub Release,"
echo "and publishes m3flow-providers $NEW to PyPI. Watch: gh run list --workflow Release"
