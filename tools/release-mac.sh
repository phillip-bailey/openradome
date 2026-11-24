#!/usr/bin/env bash
# Build the macOS binaries (Apple silicon and Intel) on a Mac and add them to the release CI made for TAG.
# CI cannot build macOS: it needs Apple's SDK. Usage: tools/release-mac.sh v0.1.0
# Needs: rustup with targets aarch64-apple-darwin and x86_64-apple-darwin, and a Gitea login in the git credential
# helper for the Gitea host (the token needs write:repository). The host and repository come from the `origin`
# remote, or from GITEA_URL (https://host) and GITEA_REPO (owner/name).
set -euo pipefail
TAG=${1:?usage: release-mac.sh vX.Y.Z}
cd "$(dirname "$0")/.."
origin=$(git remote get-url origin 2>/dev/null || true)
# ssh://git@host:2222/owner/name.git, git@host:owner/name.git or https://host/owner/name(.git)
path=$(printf '%s' "$origin" | sed -E 's#^[a-z+]+://([^@/]+@)?[^/:]+(:[0-9]+)?/##; s#^[^@]+@[^:]+:##; s#\.git$##')
host=$(printf '%s' "$origin" | sed -E 's#^[a-z+]+://([^@/]+@)?([^/:]+).*#\2#; s#^[^@]+@([^:]+):.*#\1#')
HOST=${GITEA_URL:-https://$host}
REPO=${GITEA_REPO:-$path}
[ -n "$REPO" ] && [ "$HOST" != https:// ] || { echo "set GITEA_URL and GITEA_REPO (no usable origin remote)" >&2; exit 1; }
API=$HOST/api/v1/repos/$REPO
TOKEN=$(printf 'protocol=https\nhost=%s\n\n' "${HOST#https://}" | git credential fill | sed -n 's/^password=//p')
[ -n "$TOKEN" ] || { echo "no Gitea credential for ${HOST#https://}" >&2; exit 1; }
auth="Authorization: token $TOKEN"

ID=$(curl -fsS -H "$auth" "$API/releases/tags/$TAG" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])') ||
  { echo "release $TAG does not exist yet: wait for CI to publish it" >&2; exit 1; }

# Build the tagged source, not whatever is checked out.
src=$(mktemp -d); trap 'rm -rf "$src"' EXIT
git archive "$TAG" | tar -x -C "$src"
out=$(mktemp -d); trap 'rm -rf "$src" "$out"' EXIT

for triple in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add "$triple" >/dev/null
  (cd "$src" && cargo build --release --locked --target "$triple")
  name=rustadsb-$TAG-$triple
  mkdir "$out/$name"
  cp "$src/target/$triple/release/rustadsb" "$src/README.md" "$src/COPYING" "$src/NOTICE" "$out/$name/"
  tar -C "$out" -czf "$out/$name.tar.gz" "$name"
  rm -rf "$out/${name:?}"
done

# Replace any earlier macOS assets, then rebuild SHA256SUMS from every archive on the release.
assets=$(curl -fsS -H "$auth" "$API/releases/$ID/assets")
for a in $(printf '%s' "$assets" | python3 -c 'import json,sys
for a in json.load(sys.stdin):
    if "apple-darwin" in a["name"] or a["name"]=="SHA256SUMS": print(a["id"])'); do
  curl -fsS -X DELETE -H "$auth" "$API/releases/$ID/assets/$a"
done
for f in "$out"/*.tar.gz; do
  curl -fsS -X POST -H "$auth" -F "attachment=@$f" "$API/releases/$ID/assets?name=$(basename "$f")" >/dev/null
  echo "uploaded $(basename "$f")"
done
: > "$out/SHA256SUMS"
for url in $(curl -fsS -H "$auth" "$API/releases/$ID/assets" | python3 -c 'import json,sys
for a in json.load(sys.stdin): print(a["browser_download_url"])'); do
  n=$(basename "$url")
  curl -fsSL -H "$auth" -o "$out/dl-$n" "$url"
  (cd "$out" && shasum -a 256 "dl-$n" | sed "s/dl-//" >> SHA256SUMS)
done
curl -fsS -X POST -H "$auth" -F "attachment=@$out/SHA256SUMS" "$API/releases/$ID/assets?name=SHA256SUMS" >/dev/null
echo "SHA256SUMS regenerated:"; cat "$out/SHA256SUMS"
