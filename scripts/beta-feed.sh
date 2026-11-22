#!/usr/bin/env bash
# The beta channel's feed: the latest.json of the fixed release `beta-channel`.
#   beta-feed.sh update <latest.json> <tag>   put a published release's own latest.json there, if it is newer
# release.yml's beta-feed job runs it, with gh signed in (GH_TOKEN) and GITHUB_REPOSITORY and GITHUB_SHA set.
# The feed only goes up, for betas and stable releases alike, and whatever can't be read stops the script before
# it changes anything. Sourced, it only defines its functions; scripts/beta-feed-test.sh tests them.

# A version the feed may hold: X.Y.Z or X.Y.Z-beta.N.
on_channel() {
  [[ "$1" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-beta\.(0|[1-9][0-9]*))?$ ]]
}

# newer A B: A is a later version than B. `sort -V` puts `~` before everything, even the end, so with the first `-`
# as `~` a beta comes before its release: 0.5.0~beta.9 < 0.5.0~beta.10 < 0.5.0 < 0.5.1~beta.1.
newer() {
  local a b
  a=$(sed 's/-/~/' <<< "$1") && b=$(sed 's/-/~/' <<< "$2") || return 2
  [ "$a" != "$b" ] && [ "$(printf '%s\n%s\n' "$a" "$b" | LC_ALL=C sort -V | tail -n1)" = "$a" ]
}

# decide NEW CURRENT: `update` or `keep` ("" for CURRENT: there is no feed yet). Fails on a version it can't order.
decide() {
  on_channel "$1" || { echo "not a version for the beta channel: '$1'" >&2; return 1; }
  [ -n "$2" ] || { echo update; return; }
  on_channel "$2" || { echo "the feed's version is not one for the beta channel: '$2'" >&2; return 1; }
  if newer "$1" "$2"; then echo update; else echo keep; fi
}

die() { echo "::error::$*" >&2; exit 1; }

# The `beta-channel` release as JSON, or nothing when GitHub says there is none (404). Fails on anything else.
read_release() {
  local out err
  err=$(mktemp) || return 1
  if out=$(gh api "repos/$GITHUB_REPOSITORY/releases/tags/beta-channel" 2> "$err"); then
    rm -f "$err"
    jq -e '(.id | type == "number") and (.assets | type == "array")' <<< "$out" > /dev/null || return 1
    printf '%s\n' "$out"
  elif grep -q '(HTTP 404)' "$err"; then
    rm -f "$err"
  else
    cat "$err" >&2; rm -f "$err"; return 1
  fi
}

# asset_id RELEASE NAME: that asset's id, or nothing.
asset_id() { jq -r --arg n "$2" '[.assets[] | select(.name == $n)][0].id // empty' <<< "$1"; }

assets="repos/${GITHUB_REPOSITORY:-}/releases/assets"
rename() { gh api -X PATCH "$assets/$1" -f name="$2" > /dev/null; }
delete() { gh api -X DELETE "$assets/$1" > /dev/null; }

update() {
  local file=$1 tag=$2 new release id="" old="" prev next cur="" tmp
  on_channel "${tag#v}" || { echo "$tag is neither a beta nor a stable release: the beta channel is unchanged"; return; }
  new=$(jq -er .version "$file") || die "could not read the version of $file"
  [ "$new" = "${tag#v}" ] || die "$file is for $new, not $tag"

  release=$(read_release) || die "could not read the beta-channel release: the beta channel is unchanged"
  if [ -n "$release" ]; then
    id=$(jq -er .id <<< "$release") || die "could not read the beta-channel release's id"
    old=$(asset_id "$release" latest.json) || die "could not read the beta-channel release's files"
    prev=$(asset_id "$release" latest-prev.json) || die "could not read the beta-channel release's files"
    next=$(asset_id "$release" latest-next.json) || die "could not read the beta-channel release's files"
    # Left by a run that stopped half way (see below).
    if [ -z "$old" ] && [ -n "$prev" ]; then
      rename "$prev" latest.json || die "could not put the previous feed back"
      old=$prev prev=""
    fi
    for leftover in $prev $next; do delete "$leftover" || die "could not delete a leftover of an earlier run"; done
  fi
  if [ -n "$old" ]; then
    cur=$(gh api -H "Accept: application/octet-stream" "$assets/$old" | jq -er .version) ||
      die "could not read the feed's version: the beta channel is unchanged"
  fi
  case "$(decide "$new" "$cur")" in
    update) ;;
    keep) echo "the beta channel is at $cur, not older than $new: unchanged"; return ;;
    *) die "could not compare $new with the feed's '$cur': the beta channel is unchanged" ;;
  esac

  if [ -z "$id" ]; then
    gh release create beta-channel -R "$GITHUB_REPOSITORY" --prerelease --latest=false --target "$GITHUB_SHA" \
      --title "Beta channel (update feed)" \
      --notes "The update feed for apps on the beta channel. Not a release: download betas from their own vX.Y.Z-beta.N release." ||
      die "could not create the beta-channel release (docs/RELEASING.md, Beta releases: create it by hand, then re-run this job)"
  fi
  # GitHub can't replace a release's file in place, so the new feed is uploaded whole beside the old one and two
  # renames swap them. Only between those two calls (or until a re-run, if the job dies there) does latest.json
  # answer 404. Clients in that moment: the app takes a 404 as "nothing on the beta feed", still offers a newer
  # stable from the stable feed, and looks again at its next check; install.sh with HBENCH_CHANNEL=beta stops with
  # "could not read the beta channel". Neither ever gets a partial or an older file.
  tmp=$(mktemp -d) || die "mktemp"
  cp "$file" "$tmp/latest-next.json" || die "could not copy $file"
  gh release upload beta-channel -R "$GITHUB_REPOSITORY" "$tmp/latest-next.json" || die "could not upload the new feed"
  rm -rf "$tmp"
  release=$(read_release) && next=$(asset_id "$release" latest-next.json) && [ -n "$next" ] ||
    die "could not find the uploaded feed"
  if [ -n "$old" ]; then rename "$old" latest-prev.json || die "could not move the old feed aside"; fi
  if ! rename "$next" latest.json; then
    if [ -n "$old" ]; then rename "$old" latest.json || echo "::warning::the old feed is still latest-prev.json: re-run this job" >&2; fi
    die "could not put the new feed in place"
  fi
  if [ -n "$old" ]; then delete "$old" || echo "::warning::could not delete latest-prev.json; the next update will" >&2; fi
  echo "beta channel: ${cur:-none} -> $new"
  [ -z "${GITHUB_STEP_SUMMARY:-}" ] || echo "beta channel: ${cur:-none} -> $new" >> "$GITHUB_STEP_SUMMARY"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  [ "${1:-}" = update ] && [ $# -eq 3 ] || { echo "usage: beta-feed.sh update <latest.json> <tag>" >&2; exit 2; }
  : "${GITHUB_REPOSITORY:?}" "${GITHUB_SHA:?}"
  update "$2" "$3"
fi
