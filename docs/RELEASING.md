# Releasing

## How to release

**Actions → Release → Run workflow → pick bump → approve.**

1. In [Actions → Release](https://github.com/node-q/hbench/actions/workflows/release-start.yml), click **Run
   workflow** on `master`. Pick `patch`, `minor` or `major` (counted from the latest `vX.Y.Z` tag), or type an exact
   `version` such as `0.4.0` or `0.4.0-rc.1`. Tick **dry run** to see the version, the bump and the release notes
   in the run summary without changing anything.
2. The run opens a **Release x.y.z** pull request that bumps the version in `Cargo.toml`, `Cargo.lock` and the
   desktop app's files, lists the pull requests merged since the last release, and runs CI on it.
3. **Approve that pull request.** That is the one approval: once CI has passed, the run merges it, tags `vX.Y.Z` on
   the merge commit and starts the release build (`release.yml`) on that tag.
4. The run's summary ends with the release's URL and its files once the build has published it.

The run refuses when the tag already exists, when the version is not newer than the latest release, or when
master's latest CI run did not pass. Close the pull request (and delete its `release/x.y.z` branch) to cancel.
If `master` already has the version but no tag, say a release pull request was merged by hand and never tagged,
the run skips the pull request and tags `master`. The pull request waits up to about six hours for approval;
after that, merge it and run Release again with the same version, which then just tags and releases.

### What the workflow needs from the repository settings

- **Settings → Actions → General → "Allow GitHub Actions to create and approve pull requests"** must be on, or the
  run cannot open the Release pull request. The workflow itself never approves anything.
- `master`'s ruleset requires an approval from a code owner (`.github/CODEOWNERS`), so only they can approve the
  Release pull request.
- The `release` environment keeps its rule that only `v*` tags may deploy to it. Add required reviewers there only
  if you want a second approval before publishing; each job in that environment then waits for one.
- The `release` environment holds the desktop app's signing secrets: the Apple ones (see
  [desktop/README.md](../desktop/README.md#releases)) and the in-app updater's key, `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. Without the updater key a release still ships, but with no `latest.json`, so
  installed apps are not offered it.

### By hand

The workflow automates these steps, which still work: run `scripts/set-version.sh X.Y.Z` (it writes hbench's and the
desktop app's version files), merge that in a "Release X.Y.Z" pull request, then tag the merge commit:

```bash
git tag -a vX.Y.Z -m "hbench vX.Y.Z" && git push origin vX.Y.Z
```

## What the release build does

`.github/workflows/release.yml` checks the tag against `Cargo.toml` and, on GitHub-hosted runners, builds static
x86_64/aarch64 Linux (musl) and x86_64/arm64 macOS binaries, one per job. It publishes only once `ci.yml` has passed
on the tagged commit (unit tests on Linux and Windows, E2E, lint, audit), waiting for that run if it is still going,
so a tag on a commit that is not on `master`, that CI never ran on, or that CI failed, publishes nothing. The CI run
is the push to `master`'s, or one the Release workflow starts on the tag when it merged the pull request itself
(a merge made with the workflow's token starts no push run). Each `.tar.gz` gets a build provenance
attestation; check one with `gh attestation verify hbench-v0.2.0-<target>.tar.gz -R node-q/hbench`. A separate publish
job, in the `release` environment, creates the GitHub release with the archives, the tagged commit's `scripts/install.sh`
(so `releases/latest/download/install.sh` is always the latest release's), `SHA256SUMS` and generated notes.
`SHA256SUMS` lists the archives and `install.sh`, and the publish job attests `install.sh` and `SHA256SUMS` too, so
`gh attestation verify install.sh -R node-q/hbench` checks the script and `sha256sum -c` checks any file against the
sums. The install script checks the archive it downloads against `SHA256SUMS` before unpacking it. Tags
with a suffix (`v0.2.0-rc.1`) are published as prereleases, which the install script skips unless `HBENCH_VERSION`
names one, and which the desktop app never offers as an update (it reads `releases/latest/download/latest.json`).
