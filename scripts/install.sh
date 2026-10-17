#!/bin/sh
# Install the latest hbench release (or HBENCH_VERSION=vX.Y.Z) into HBENCH_INSTALL_DIR (default ~/.local/bin).
#   curl -fsSL https://raw.githubusercontent.com/sapphirestd/harness-benchmark/master/scripts/install.sh | sh
# Uses `gh` when present (works for private repos), else plain curl against the public release URLs.
set -eu
REPO=sapphirestd/harness-benchmark
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

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  tag=${HBENCH_VERSION:-$(gh release view -R "$REPO" --json tagName -q .tagName)}
  gh release download "$tag" -R "$REPO" -D "$tmp" -p "hbench-$tag-$target.tar.gz" -p SHA256SUMS
else
  # /releases/latest redirects to /releases/tag/<tag>; reading the redirect avoids the API rate limit.
  tag=${HBENCH_VERSION:-$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's|.*/||')}
  for f in "hbench-$tag-$target.tar.gz" SHA256SUMS; do
    curl -fsSL -o "$tmp/$f" "https://github.com/$REPO/releases/download/$tag/$f"
  done
fi

cd "$tmp"
grep " hbench-$tag-$target.tar.gz\$" SHA256SUMS > sum
if command -v sha256sum >/dev/null 2>&1; then sha256sum -c sum >/dev/null; else shasum -a 256 -c sum >/dev/null; fi
tar -xzf "hbench-$tag-$target.tar.gz"
mkdir -p "$DIR"
install -m 755 "hbench-$tag-$target/hbench" "$DIR/hbench"
echo "installed hbench $tag to $DIR/hbench"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "add $DIR to your PATH" ;; esac
