#!/bin/sh
# Install hbench into HBENCH_INSTALL_DIR (default ~/.local/bin).
#   curl -fsSL https://github.com/node-q/hbench/releases/latest/download/install.sh | sh
# HBENCH_VERSION=vX.Y.Z pins a release; the default is the latest release of node-q/hbench.
# HBENCH_CHANNEL=beta installs the beta channel's release instead (the version in the `beta-channel` release's
# latest.json: the newest beta, or a newer stable, whose release updated it; if a release's update of it failed, it
# can be behind stable until that job is re-run). HBENCH_VERSION still wins over it.
set -eu
REPO=node-q/hbench
# A site serving this script may fill in a default release here; HBENCH_VERSION still overrides it.
DEFAULT_VERSION=
DIR=${HBENCH_INSTALL_DIR:-$HOME/.local/bin}
CHANNEL=${HBENCH_CHANNEL:-stable}
case "$CHANNEL" in stable | beta) ;; *) echo "unknown HBENCH_CHANNEL: $CHANNEL (use stable or beta)" >&2; exit 1 ;; esac

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# /releases/latest redirects to /releases/tag/<tag>; reading the redirect, without fetching that page, avoids the API rate limit.
if [ -z "${HBENCH_VERSION:-}" ] && [ "$CHANNEL" = beta ]; then
  beta=$(curl -fsSL "https://github.com/$REPO/releases/download/beta-channel/latest.json" | sed -n 's/.*"version" *: *"\([0-9][0-9A-Za-z.-]*\)".*/\1/p' | head -n1)
  [ -n "$beta" ] || { echo "could not read the beta channel of $REPO" >&2; exit 1; }
  HBENCH_VERSION="v${beta#v}"
fi
tag=${HBENCH_VERSION:-${DEFAULT_VERSION:-$(curl -fsSI -o /dev/null -w '%{redirect_url}' "https://github.com/$REPO/releases/latest" | sed 's|.*/||')}}
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
