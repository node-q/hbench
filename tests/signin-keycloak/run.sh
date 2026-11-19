#!/bin/sh
# Starts Keycloak 26.8.2 with realm-market.json and runs e2e.sh in an Arch Linux container with gnome-keyring,
# against target/debug/hbench. Usage, from the repository root: cargo build && sh tests/signin-keycloak/run.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
docker rm -f hbench-signin-kc >/dev/null 2>&1 || true
docker run -d --name hbench-signin-kc -p 127.0.0.1:8180:8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=admin \
  -v "$HERE/realm-market.json:/opt/keycloak/data/import/realm-market.json:ro" \
  quay.io/keycloak/keycloak:26.8.2 start-dev --import-realm --http-port=8080 --hostname=http://127.0.0.1:8180 >/dev/null
trap 'docker rm -f hbench-signin-kc >/dev/null 2>&1 || true' EXIT
i=0
until curl -sf http://127.0.0.1:8180/realms/market/.well-known/openid-configuration >/dev/null; do
  i=$((i + 1)); [ $i -lt 90 ] || { echo "Keycloak did not start"; exit 1; }; sleep 2
done
docker run --rm --network host -v "$HERE:/e2e:ro" -v "$ROOT/target/debug/hbench:/hbench:ro" archlinux:latest sh -c '
  pacman -Sy --noconfirm --needed python gnome-keyring libsecret dbus curl >/dev/null 2>&1
  useradd -m e2e
  # gnome-keyring refuses to run as root; the keyring is unlocked with a throwaway password.
  su e2e -c "cd && dbus-run-session -- sh -c \"echo e2e | gnome-keyring-daemon --unlock --components=secrets >/dev/null; sh /e2e/e2e.sh\""'
