#!/bin/sh
# Install hbench into HBENCH_INSTALL_DIR (default ~/.local/bin).
#   curl -fsSL https://nodeq.tech/install.sh | sh
# HBENCH_VERSION=vX.Y.Z pins a release; the default is the latest release of
# node-q/hbench-releases. If that download fails, it falls back to the releases of the
# node-q/hbench source repo.
set -eu
RELEASES_REPO=node-q/hbench-releases
SOURCE_REPO=node-q/hbench
# nodeq.tech/install.sh fills this in with the version Harness Playground's arenas use.
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

# download REPO: fetch the release archive and its SHA256SUMS from REPO into $tmp.
download() {
  # /releases/latest redirects to /releases/tag/<tag>; reading the redirect avoids the API rate limit.
  tag=${HBENCH_VERSION:-${DEFAULT_VERSION:-$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$1/releases/latest" | sed 's|.*/||')}}
  case "$tag" in v*) ;; *) return 1 ;; esac
  for f in "hbench-$tag-$target.tar.gz" SHA256SUMS; do
    curl -fsSL -o "$tmp/$f" "https://github.com/$1/releases/download/$tag/$f" || return 1
  done
}

if ! download "$RELEASES_REPO"; then
  echo "download from $RELEASES_REPO failed; trying $SOURCE_REPO" >&2
  download "$SOURCE_REPO" || { echo "could not download hbench ${HBENCH_VERSION:-$DEFAULT_VERSION}" >&2; exit 1; }
fi

cd "$tmp"
grep " hbench-$tag-$target.tar.gz\$" SHA256SUMS > sum || { echo "SHA256SUMS of $tag has no entry for hbench-$tag-$target.tar.gz" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then sha256sum -c sum >/dev/null; else shasum -a 256 -c sum >/dev/null; fi
tar -xzf "hbench-$tag-$target.tar.gz"
mkdir -p "$DIR"
install -m 755 "hbench-$tag-$target/hbench" "$DIR/hbench"
echo "installed hbench $tag to $DIR/hbench"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "add $DIR to your PATH" ;; esac
