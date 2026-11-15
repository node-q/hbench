#!/bin/sh
# Builds hbench and puts it where Tauri bundles it: src-tauri/binaries/hbench-<target triple>.
# `release` (the default) for packaged builds, `debug` for `npm run dev`.
set -eu
profile="${1:-release}"
root="$(cd "$(dirname "$0")/../.." && pwd)"
triple="$(rustc -vV | sed -n 's/^host: //p')"
cd "$root"
if [ "$profile" = release ]; then
  cargo build --release --locked
else
  cargo build --locked
fi
ext=""
case "$triple" in *windows*) ext=".exe" ;; esac
mkdir -p desktop/src-tauri/binaries
cp "target/$profile/hbench$ext" "desktop/src-tauri/binaries/hbench-$triple$ext"
echo "sidecar: desktop/src-tauri/binaries/hbench-$triple$ext ($profile)"
