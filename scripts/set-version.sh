#!/bin/sh
# Sets the version everywhere this repository writes it down: hbench's Cargo.toml and Cargo.lock, and the
# Actuaria desktop app's (desktop/), which ships with each hbench release under the same version.
# Run from anywhere in the repository: scripts/set-version.sh 0.4.0
# The Release workflow (.github/workflows/release-start.yml) runs it to make the "Release x.y.z" commit.
set -eu
version="${1:?usage: scripts/set-version.sh <version>}"
case "$version" in *[!0-9A-Za-z.-]*) echo "not a semver version: $version" >&2; exit 1 ;; esac
echo "$version" | grep -Eqx '(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?' ||
  { echo "not a semver version: $version" >&2; exit 1; }
root="$(git rev-parse --show-toplevel)"
app="$root/desktop/src-tauri"
export VERSION="$version"

# A package's own version is the first `version = "..."` in its Cargo.toml, and in Cargo.lock the line after
# its `name = "..."`; the app's is the first "version" in tauri.conf.json.
perl -0pi -e 's/^version = "[^"]*"/version = "$ENV{VERSION}"/m' "$root/Cargo.toml" "$app/Cargo.toml"
perl -0pi -e 's/^(name = "hbench"\nversion = )"[^"]*"/$1"$ENV{VERSION}"/m' "$root/Cargo.lock"
perl -0pi -e 's/^(name = "actuaria-desktop"\nversion = )"[^"]*"/$1"$ENV{VERSION}"/m' "$app/Cargo.lock"
perl -0pi -e 's/"version": "[^"]*"/"version": "$ENV{VERSION}"/' "$app/tauri.conf.json"

# Fail rather than leave a file behind: each must now say the new version.
check() { grep -Fqx "$2" "$1" || { echo "set-version: $1 does not have $2" >&2; exit 1; }; }
check "$root/Cargo.toml" "version = \"$version\""
check "$app/Cargo.toml" "version = \"$version\""
grep -A1 -Fx 'name = "hbench"' "$root/Cargo.lock" | grep -Fqx "version = \"$version\"" ||
  { echo "set-version: Cargo.lock does not have hbench $version" >&2; exit 1; }
grep -A1 -Fx 'name = "actuaria-desktop"' "$app/Cargo.lock" | grep -Fqx "version = \"$version\"" ||
  { echo "set-version: desktop/src-tauri/Cargo.lock does not have actuaria-desktop $version" >&2; exit 1; }
check "$app/tauri.conf.json" "  \"version\": \"$version\","
echo "version: $version"
