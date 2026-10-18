#!/bin/sh
# Install hbench into HBENCH_INSTALL_DIR (default ~/.local/bin).
#   curl -fsSL https://nodeq.tech/install.sh | sh
# HBENCH_VERSION=vX.Y.Z pins a release; the default is the latest release of the public
# node-q/hbench-releases repo. If that download fails and `gh` is signed in, it falls back to
# the private node-q/harness-benchmark releases.
set -eu
PUBLIC_REPO=node-q/hbench-releases
PRIVATE_REPO=node-q/harness-benchmark
# nodeq.tech/install.sh fills this in with the version the arenas pin.
DEFAULT_VERSION=
DIR=${HBENCH_INSTALL_DIR:-$HOME/.local/bin}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

from_public() {
  # /releases/latest redirects to /releases/tag/<tag>; reading the redirect avoids the API rate limit.
  tag=${HBENCH_VERSION:-${DEFAULT_VERSION:-$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$PUBLIC_REPO/releases/latest" | sed 's|.*/||')}}
  case "$tag" in v*) ;; *) return 1 ;; esac
  for f in "hbench-$tag-$target.tar.gz" SHA256SUMS; do
    curl -fsSL -o "$tmp/$f" "https://github.com/$PUBLIC_REPO/releases/download/$tag/$f" || return 1
  done
}

from_private() {
  command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1 || return 1
  tag=${HBENCH_VERSION:-${DEFAULT_VERSION:-$(gh release view -R "$PRIVATE_REPO" --json tagName -q .tagName)}}
  gh release download "$tag" -R "$PRIVATE_REPO" -D "$tmp" --clobber -p "hbench-$tag-$target.tar.gz" -p SHA256SUMS
}

if ! from_public; then
  echo "download from $PUBLIC_REPO failed; trying $PRIVATE_REPO with gh" >&2
  from_private || { echo "could not download hbench ${HBENCH_VERSION:-$DEFAULT_VERSION}" >&2; exit 1; }
fi

cd "$tmp"
grep " hbench-$tag-$target.tar.gz\$" SHA256SUMS > sum || { echo "SHA256SUMS of $tag has no entry for hbench-$tag-$target.tar.gz" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then sha256sum -c sum >/dev/null; else shasum -a 256 -c sum >/dev/null; fi
tar -xzf "hbench-$tag-$target.tar.gz"
mkdir -p "$DIR"
install -m 755 "hbench-$tag-$target/hbench" "$DIR/hbench"
echo "installed hbench $tag to $DIR/hbench"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "add $DIR to your PATH" ;; esac
