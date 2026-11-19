#!/bin/sh
# End-to-end: `hbench login --browser` against a real Keycloak (26.8.2, realm `market`, client
# `essentials-desktop` as sign-in-products.md 7.1 registers it) with a password user, the real loopback
# listener and the real OS keychain (gnome-keyring's Secret Service). Run by run.sh inside a container;
# needs a debug build of hbench (the issuer override exists only there).
set -eu
HBENCH=${HBENCH:-/hbench}
HERE=$(cd "$(dirname "$0")" && pwd)
KC=http://127.0.0.1:8180/realms/market
export HBENCH_DEV_MARKET_ISSUER=$KC HBENCH_DEV_MARKET_API=http://127.0.0.1:8181 HBENCH_NO_UPDATE_CHECK=1
fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "ok - $*"; }

# The "browser": hbench runs xdg-open; ours signs in with a password in the background.
mkdir -p /tmp/bin
printf '#!/bin/sh\n(python3 %s/browser.py "$1" >>/tmp/browser.log 2>&1 &)\n' "$HERE" >/tmp/bin/xdg-open
chmod +x /tmp/bin/xdg-open
export PATH=/tmp/bin:$PATH
python3 "$HERE/market_api.py" 8181 &

lookup() { secret-tool lookup service io.actuaria.hbench.signin username "essentials-desktop@$KC" 2>/dev/null || true; }
refresh() { curl -s -o /dev/null -w '%{http_code}' -d grant_type=refresh_token -d client_id=essentials-desktop --data-urlencode "refresh_token=$1" "$KC/protocol/openid-connect/token"; }

# Section 12, check 1: any loopback port matches the one registered redirect URI. Three sign-ins, three ports.
ports=""
for i in 1 2 3; do
  "$HBENCH" login --browser >/tmp/login.out 2>&1 || { cat /tmp/login.out /tmp/browser.log; fail "sign-in $i"; }
  port=$(sed -n 's/.*redirect_uri=http%3A%2F%2F127.0.0.1%3A\([0-9]*\)%2Fcallback.*/\1/p' /tmp/login.out)
  grep -q '"azp": "essentials-desktop"' /tmp/login.out || { cat /tmp/login.out; fail "whoami after sign-in $i"; }
  ports="$ports $port"
done
[ "$(echo $ports | tr ' ' '\n' | sort -u | wc -l)" = 3 ] || fail "three ports expected: $ports"
pass "three sign-ins through ports$ports, each followed by whoami with an essentials-desktop token"

# localhost and another path are refused by Keycloak.
code() { curl -s -o /dev/null -w '%{http_code}' "$KC/protocol/openid-connect/auth?response_type=code&client_id=essentials-desktop&scope=openid&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256&state=s&redirect_uri=$1"; }
[ "$(code 'http%3A%2F%2F127.0.0.1%3A5555%2Fcallback')" = 200 ] || fail "127.0.0.1 on another port"
[ "$(code 'http%3A%2F%2Flocalhost%3A5555%2Fcallback')" = 400 ] || fail "localhost accepted"
[ "$(code 'http%3A%2F%2F127.0.0.1%3A5555%2Fother')" = 400 ] || fail "another path accepted"
[ "$(code 'http%3A%2F%2F127.0.0.1%2Fcallback%2Fx')" = 400 ] || fail "a longer path accepted"
pass "Keycloak refuses localhost and other paths, accepts 127.0.0.1 on any port"

# The refresh token is in the keychain (and nowhere in hbench's files).
rt1=$(lookup)
[ -n "$rt1" ] || fail "no refresh token in the keychain"
! grep -rqF "$rt1" "$HOME/.cache/hbench" 2>/dev/null || fail "the refresh token is in a file"
pass "the refresh token is in the OS keychain only"

# A new process refreshes (rotating the token); the replaced token is refused by Keycloak afterwards.
"$HBENCH" whoami >/tmp/whoami.out 2>&1 || { cat /tmp/whoami.out; fail "whoami"; }
rt2=$(lookup)
[ -n "$rt2" ] && [ "$rt2" != "$rt1" ] || fail "the refresh token did not rotate"
[ "$(refresh "$rt1")" = 400 ] || fail "the rotated-out token still works"
pass "refresh rotates the token, and the old one is refused"
# That replay was refused, and Keycloak's reuse rule then ends the session: hbench says it is signed out.
if "$HBENCH" whoami >/tmp/whoami2.out 2>&1; then
  echo "note: the session survived a replay of its old token"
else
  grep -q "signed out" /tmp/whoami2.out || { cat /tmp/whoami2.out; fail "unexpected error after a replay"; }
  [ -z "$(lookup)" ] || fail "the keychain still holds a refused token"
  pass "after a replayed token, hbench reports signed out and clears the keychain"
fi

# Sign in again, then sign out: the token is revoked at Keycloak and gone from the keychain.
"$HBENCH" login --browser >/tmp/login.out 2>&1 || { cat /tmp/login.out /tmp/browser.log; fail "sign-in before sign-out"; }
rt=$(lookup)
"$HBENCH" logout >/tmp/logout.out 2>&1 || { cat /tmp/logout.out; fail "logout"; }
[ -z "$(lookup)" ] || fail "the keychain still holds the token"
[ "$(refresh "$rt")" = 400 ] || fail "the token works after logout"
"$HBENCH" whoami >/tmp/whoami.out 2>&1 && fail "whoami after logout"
grep -q "not signed in" /tmp/whoami.out || { cat /tmp/whoami.out; fail "whoami after logout says"; }
pass "logout revokes the token at Keycloak and clears the keychain"
echo "all passed"
