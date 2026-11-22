#!/usr/bin/env bash
# Tests scripts/beta-feed.sh (and install.sh's channel check) without the network: the version ordering, and the
# update itself against a fake gh that keeps the beta-channel release in a directory. CI runs it in the Lint job.
set -uo pipefail
cd "$(dirname "$0")/.."
# shellcheck source=scripts/beta-feed.sh
. scripts/beta-feed.sh
fails=0
is() { # <what> <expected> <got>
  [ "$2" = "$3" ] || { echo "FAIL: $1: expected '$2', got '$3'"; fails=$((fails + 1)); }
}

# Each version is newer than every one before it.
order=(0.4.9 0.5.0-beta.2 0.5.0-beta.9 0.5.0-beta.10 0.5.0 0.5.9 0.5.10-beta.1 0.5.10 0.10.0-beta.1 0.10.0 1.0.0)
for i in "${!order[@]}"; do
  for j in "${!order[@]}"; do
    if [ "$i" -gt "$j" ]; then want=update; else want=keep; fi
    is "decide ${order[i]} ${order[j]}" "$want" "$(decide "${order[i]}" "${order[j]}")"
  done
done
is "no feed yet" update "$(decide 0.5.0-beta.1 "")"
for bad in "" 0.5 0.5.0-rc.1 0.5.0-beta 0.5.0-beta.01 v0.5.0 null "0.5.0 x" $'0.5.0\n0.6.0'; do
  is "new version '$bad'" fails "$(decide "$bad" 0.4.0 2> /dev/null || echo fails)"
  [ -z "$bad" ] || is "feed version '$bad'" fails "$(decide 0.5.0 "$bad" 2> /dev/null || echo fails)"
done

# The fake gh: $S/release exists when the release does (JSON in it is added to what gh says of it), $S/assets/<id>.<name> are its files, and each line of
# $S/fail names a call to fail (view, read, create, upload, rename:<new name>, delete).
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat > "$work/bin/gh" <<'GH'
#!/usr/bin/env bash
fail() { if grep -qx -- "$1" "$S/fail" 2> /dev/null; then echo "gh: ${2:-Server Error (HTTP 502)}" >&2; exit 1; fi; }
file() { for f in "$S"/assets/"$1".*; do [ -f "$f" ] && echo "$f" && return; done; echo "gh: Not Found (HTTP 404)" >&2; exit 1; }
all="$*" last="${!#}"
echo "$all" >> "$S/calls"
case "$all" in
  "api repos/o/r/releases/tags/beta-channel")
    fail view
    [ -f "$S/release" ] || { echo '{"message":"Not Found"}'; echo "gh: Not Found (HTTP 404)" >&2; exit 1; }
    for f in "$S"/assets/*; do [ -f "$f" ] && b=${f##*/} && printf '{"id":%s,"name":"%s"}\n' "${b%%.*}" "${b#*.}"; done |
      jq -s --arg pre "$(cat "$S/release")" '{id: 7, assets: .} + ($pre | if . == "" then {prerelease: true} else fromjson end)' ;;
  "api -H Accept: application/octet-stream "*) fail read "Not Found (HTTP 404)"; cat "$(file "${last##*/}")" ;;
  "api -X PATCH "*) fail "rename:${last##*name=}"; id=${4##*/}; f=$(file "$id") || exit 1; mv "$f" "$S/assets/$id.${last##*name=}" ;;
  "api -X DELETE "*) fail delete; f=$(file "${4##*/}") || exit 1; rm "$f" ;;
  "release create beta-channel "*) fail create; touch "$S/release" ;;
  "release upload beta-channel "*) fail upload; cp "$last" "$S/assets/$((100 + $(wc -l < "$S/calls"))).${last##*/}" ;;
  *) echo "unexpected gh $*" >&2; exit 1 ;;
esac
GH
chmod +x "$work/bin/gh"
export S GITHUB_REPOSITORY=o/r GITHUB_SHA=abc
unset GITHUB_STEP_SUMMARY
scene() { # <feed version or "", or "none" for no release>: a fresh state
  S=$(mktemp -d -p "$work"); mkdir "$S/assets"; : > "$S/calls"
  [ "$1" = none ] && return
  touch "$S/release"
  [ -z "$1" ] || echo "{\"version\":\"$1\"}" > "$S/assets/1.latest.json"
}
run() { # <version> [tag]: ok or failed
  echo "{\"version\":\"$1\"}" > "$S/new.json"
  if PATH="$work/bin:$PATH" scripts/beta-feed.sh update "$S/new.json" "${2:-v$1}" > "$S/log" 2>&1; then echo ok; else echo failed; fi
}
state() { for f in "$S"/assets/*; do [ -f "$f" ] && echo "$(basename "$f" | cut -d. -f2-)=$(jq -r .version "$f")"; done | LC_ALL=C sort | paste -sd' '; }
case_() { # <what> <expected result> <expected files> <version> [tag]
  is "$1: result" "$2" "$(run "$4" "${5:-}")"
  is "$1: files" "$3" "$(state)"
}

scene none; case_ "first beta, no release" ok latest.json=0.5.0-beta.1 0.5.0-beta.1
scene "";   case_ "release without a feed" ok latest.json=0.5.0-beta.1 0.5.0-beta.1
scene 0.5.0-beta.1; case_ "next beta" ok latest.json=0.5.0-beta.2 0.5.0-beta.2
scene 0.5.0-beta.10; case_ "older beta" ok latest.json=0.5.0-beta.10 0.5.0-beta.9
scene 0.5.0; case_ "beta of an already stable version" ok latest.json=0.5.0 0.5.0-beta.3
scene 0.5.0-beta.2; case_ "stable after its beta" ok latest.json=0.5.0 0.5.0
scene 0.6.0-beta.1; case_ "stable behind the beta" ok latest.json=0.6.0-beta.1 0.5.1
scene 0.5.0; case_ "re-run of the same version" ok latest.json=0.5.0 0.5.0
scene 0.5.0; case_ "release candidate" ok latest.json=0.5.0 0.6.0-rc.1
scene 0.5.0; case_ "manifest of another version" failed latest.json=0.5.0 0.6.0 v0.6.1
scene 0.5.0; echo view > "$S/fail"; case_ "release unreadable" failed latest.json=0.5.0 0.6.0
scene none; echo view > "$S/fail"; case_ "release unreadable, none there" failed "" 0.6.0
is "release unreadable: nothing is written" 1 "$(wc -l < "$S/calls")"
# A 404 on a file the release lists is a failed read, not "no feed yet".
scene 0.5.0; echo read > "$S/fail"; case_ "feed unreadable" failed latest.json=0.5.0 0.6.0
for pre in '{"prerelease":false}' '{"prerelease":null}' '{}'; do
  scene 0.5.0; echo "$pre" > "$S/release"; case_ "release not a prerelease, $pre" failed latest.json=0.5.0 0.6.0
  is "release not a prerelease, $pre: nothing is written" 1 "$(wc -l < "$S/calls")"
done
scene null; case_ "feed with a broken version" failed latest.json=null 0.6.0
scene ""; echo "not json" > "$S/assets/1.latest.json"; is "feed not JSON" failed "$(run 0.6.0)"
is "feed not JSON: kept" "not json" "$(cat "$S/assets/1.latest.json")"
scene none; echo create > "$S/fail"; case_ "release can't be created" failed "" 0.6.0
scene 0.5.0; echo upload > "$S/fail"; case_ "upload fails" failed latest.json=0.5.0 0.6.0
scene 0.5.0; echo rename:latest-prev.json > "$S/fail"; case_ "first rename fails" failed "latest-next.json=0.6.0 latest.json=0.5.0" 0.6.0
rm "$S/fail"; case_ "... then a re-run" ok latest.json=0.6.0 0.6.0
scene 0.5.0; echo rename:latest.json > "$S/fail"; case_ "second rename fails" failed "latest-next.json=0.6.0 latest-prev.json=0.5.0" 0.6.0
cp "$S/fail" "$S/fail.was"; rm "$S/fail"; case_ "... then an older release" ok latest.json=0.5.0 0.4.0
cp "$S/fail.was" "$S/fail"; is "second rename fails again" failed "$(run 0.6.0)"
rm "$S/fail"; case_ "... then a re-run" ok latest.json=0.6.0 0.6.0
scene 0.5.0; echo delete > "$S/fail"; case_ "old feed can't be deleted" ok "latest-prev.json=0.5.0 latest.json=0.6.0" 0.6.0
rm "$S/fail"; case_ "... then the next release" ok latest.json=0.7.0 0.7.0

is "install.sh, unknown channel" "unknown HBENCH_CHANNEL: nightly (use stable or beta)" "$(HBENCH_CHANNEL=nightly sh scripts/install.sh 2>&1)"

[ "$fails" -eq 0 ] || { echo "$fails failed"; exit 1; }
echo "beta-feed: all tests passed"
