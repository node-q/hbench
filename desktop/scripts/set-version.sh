#!/bin/sh
# Sets the app's version (About box, status bar, Info.plist): release builds take the hbench tag's.
set -eu
version="${1:?usage: set-version.sh <version>}"
dir="$(cd "$(dirname "$0")/../src-tauri" && pwd)"
# The first `version = "..."` in Cargo.toml is the package's; the first "version" in tauri.conf.json the app's.
perl -0pi -e 's/^version = "[^"]*"/version = "'"$version"'"/m' "$dir/Cargo.toml"
perl -0pi -e 's/"version": "[^"]*"/"version": "'"$version"'"/' "$dir/tauri.conf.json"
echo "desktop version: $version"
