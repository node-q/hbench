#!/bin/sh
# Install hbench into HBENCH_INSTALL_DIR (default ~/.local/bin).
#   curl -fsSL https://github.com/node-q/hbench/releases/latest/download/install.sh | sh
# HBENCH_VERSION=vX.Y.Z pins a release; the default is the latest release of node-q/hbench.
set -eu
REPO=node-q/hbench
# A site serving this script may fill in a default release here; HBENCH_VERSION still overrides it.
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

# /releases/latest redirects to /releases/tag/<tag>; reading the redirect avoids the API rate limit.
tag=${HBENCH_VERSION:-${DEFAULT_VERSION:-$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's|.*/||')}}
case "$tag" in v*) ;; *) echo "could not find the latest hbench release of $REPO" >&2; exit 1 ;; esac
for f in "hbench-$tag-$target.tar.gz" SHA256SUMS; do
  curl -fsSL -o "$tmp/$f" "https://github.com/$REPO/releases/download/$tag/$f" || { echo "could not download $f of hbench $tag from $REPO" >&2; exit 1; }
done

cd "$tmp"
grep " hbench-$tag-$target.tar.gz\$" SHA256SUMS > sum || { echo "SHA256SUMS of $tag has no entry for hbench-$tag-$target.tar.gz" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then sha256sum -c sum >/dev/null; else shasum -a 256 -c sum >/dev/null; fi
tar -xzf "hbench-$tag-$target.tar.gz"
mkdir -p "$DIR"
install -m 755 "hbench-$tag-$target/hbench" "$DIR/hbench"
echo "installed hbench $tag to $DIR/hbench"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "add $DIR to your PATH" ;; esac
