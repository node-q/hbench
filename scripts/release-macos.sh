#!/usr/bin/env bash
# Build the macOS binaries and add them to an existing GitHub release (created by
# .github/workflows/release.yml, whose self-hosted runner is Linux-only).
#   scripts/release-macos.sh v0.1.0
set -euo pipefail
cd "$(dirname "$0")/.."
TAG=${1:?usage: $0 vX.Y.Z}
[ "$(uname -s)" = Darwin ] || { echo "run on macOS"; exit 1; }
git fetch -q origin tag "$TAG"
[ "$(git rev-parse HEAD)" = "$(git rev-parse "$TAG^{commit}")" ] || { echo "check out $TAG first (HEAD is not the tagged commit)"; exit 1; }
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "working tree has changes"; exit 1; }

DIST=$(mktemp -d)
# Keep the home directory (checkout, cargo registry, toolchain) out of panic and debug paths.
export RUSTFLAGS="--remap-path-prefix=$HOME=/build --remap-path-prefix=$PWD=/build/hbench"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  rustup target add "$target" >/dev/null
  cargo build --release --locked --target "$target"
  name="hbench-$TAG-$target"
  mkdir "$DIST/$name"
  cp "target/$target/release/hbench" README.md LICENSE NOTICE "$DIST/$name/"
  tar -C "$DIST" -czf "$DIST/$name.tar.gz" "$name"
done

# Keep the release's Linux entries in SHA256SUMS; replace any macOS ones (safe to re-run).
gh release download "$TAG" -p SHA256SUMS -O - | grep -v -- '-apple-darwin\.tar\.gz$' > "$DIST/SHA256SUMS" || true
(cd "$DIST" && shasum -a 256 -- hbench-*-apple-darwin.tar.gz >> SHA256SUMS && sort -k2 -o SHA256SUMS SHA256SUMS)
gh release upload "$TAG" "$DIST"/hbench-*-apple-darwin.tar.gz "$DIST/SHA256SUMS" --clobber
echo "uploaded macOS binaries to $TAG"

# The Linux release job created the public mirror release; add the macOS archives and the full SHA256SUMS.
if gh release view "$TAG" -R node-q/hbench-releases >/dev/null 2>&1; then
  gh release upload "$TAG" -R node-q/hbench-releases "$DIST"/hbench-*-apple-darwin.tar.gz "$DIST/SHA256SUMS" --clobber
  echo "mirrored macOS binaries to node-q/hbench-releases"
fi
