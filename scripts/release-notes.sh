#!/bin/sh
# Lists the pull requests merged into master between a release tag and a commit, oldest first, one line each:
#   - Title (#123, author)
# Run from anywhere in the repository, with gh signed in (or GH_TOKEN set): scripts/release-notes.sh v0.3.14 master
# The Release workflow puts this list in the "Release x.y.z" pull request.
set -eu
from="${1:?usage: scripts/release-notes.sh <previous tag> <commit>}"
to="${2:?usage: scripts/release-notes.sh <previous tag> <commit>}"
repo="${GITHUB_REPOSITORY:-node-q/hbench}"

# Merge commits say "Merge pull request #N from ..."; squash merges end in "(#N)".
numbers=$(git log --first-parent --reverse --format=%s "$from..$to" |
  sed -nE 's/^Merge pull request #([0-9]+) .*/\1/p; s/.*\(#([0-9]+)\)$/\1/p')
[ -n "$numbers" ] || { echo "- No pull requests merged since $from."; exit 0; }
for n in $numbers; do
  gh pr view "$n" -R "$repo" --json number,title,author --template '- {{.title}} (#{{.number}}, {{.author.login}}){{"\n"}}'
done
