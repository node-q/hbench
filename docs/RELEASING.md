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

## Beta releases

Trunk-based: betas and stable releases are both tags on `master` commits; there is no beta or release branch.
Unfinished work stays behind a flag so that master can always ship.

- **Cut a beta:** Actions → Release → Run workflow → `channel: beta`, then pick the bump. It computes the next
  `X.Y.Z-beta.N` (N is one more than the highest existing beta of that version), opens the "Release X.Y.Z-beta.N" PR
  as for stable (one approval), and tags its merge commit. An exact `version` such as `0.5.0-beta.3` also works.
- **Publishing:** a `-beta.N` tag is published as a prerelease and is never "latest", so the stable channel never
  sees it. After a beta or stable release is out, the `beta-feed` job puts its `latest.json` in the fixed release
  **`beta-channel`**, the beta channel's update feed:
  `https://github.com/node-q/hbench/releases/download/beta-channel/latest.json`. The feed is the release's own
  `latest.json`, signed with the same updater key, and it only goes up: the job (`scripts/beta-feed.sh`) replaces it
  only with a newer version (0.5.0 is newer than 0.5.0-beta.10), and stops without touching it when it can't read
  what is there. Other suffixes (`-rc.1`) leave it alone.
- **If the `beta-feed` job fails,** the release is out but the feed is behind, possibly behind stable. Re-run that job
  (Actions → the run → Re-run failed jobs) within 7 days, while the run still has its `dist-updater` artifact; after
  that, the next release moves the feed on. `latest-next.json` or `latest-prev.json` left in `beta-channel` is from an
  interrupted update, and the next run cleans it up.
- **Settings the feed needs:** immutable releases must stay off for `beta-channel` (they are a repository or
  organization setting), since its file is replaced. The first run creates the release and its `beta-channel` tag. If
  GitHub refuses the tag (a tag ruleset, say), create the release once by hand, as a prerelease that is not the
  latest, with no files, and re-run the job.
- **Essentials:** Help (App menu on macOS) → **Receive Beta Updates**. It is off by default and kept in the app's
  settings. On the beta channel the app reads the beta feed and the stable feed separately and offers the higher
  version, so a beta feed that is behind or unreadable never hides a newer stable. The stable feed never offers a
  prerelease. Updates only ever go up, so turning it off never downgrades: the app stays on its version until stable
  passes it. Turning it on or off drops an update that was only offered; a download or an installed update goes on.
- **`.deb` versions:** to dpkg, `0.5.0-beta.1` sorts above `0.5.0`. The in-app updater compares semver and installs
  with `dpkg -i`, which allows it, but `apt install ./….deb` of 0.5.0 over the beta counts as a downgrade and needs
  `--allow-downgrades`. The `.rpm` likely sorts the same way, and the in-app updater's `rpm -U` refuses a downgrade:
  check beta to stable on an rpm system before the first stable that follows a beta.
- **CLI:** `curl -fsSL https://github.com/node-q/hbench/releases/latest/download/install.sh | HBENCH_CHANNEL=beta sh`
  installs the beta feed's version (which can be behind stable, see above); any other `HBENCH_CHANNEL` than
  `stable` or `beta` is refused. `HBENCH_VERSION` still pins an exact release.

## What the release build does

`.github/workflows/release.yml` checks the tag against `Cargo.toml` and, on GitHub-hosted runners, builds static
x86_64/aarch64 Linux (musl) and x86_64/arm64 macOS binaries, one per job. It publishes only once `ci.yml` has passed
on the tagged commit (unit tests on Linux and Windows, E2E, lint, audit), waiting for that run if it is still going,
so a tag on a commit that is not on `master`, that CI never ran on, or that CI failed, publishes nothing. The CI run
is the push to `master`'s, or one the Release workflow starts on the tag when it merged the pull request itself
(a merge made with the workflow's token starts no push run). Each `.tar.gz` gets a build provenance
attestation; check one with `gh attestation verify hbench-v0.2.0-<target>.tar.gz -R node-q/hbench`. A separate publish
job, in the `release` environment, creates the GitHub release with the archives, the tagged commit's `scripts/install.sh`
(so `releases/latest/download/install.sh` is always the latest release's), `SHA256SUMS` and generated notes. A
stable release's notes start from the previous stable release.
`SHA256SUMS` lists the archives and `install.sh`, and the publish job attests `install.sh` and `SHA256SUMS` too, so
`gh attestation verify install.sh -R node-q/hbench` checks the script and `sha256sum -c` checks any file against the
sums. The install script checks the archive it downloads against `SHA256SUMS` before unpacking it. Tags
with a suffix (`v0.2.0-rc.1`) are published as prereleases, which the install script skips unless `HBENCH_VERSION`
names one or `HBENCH_CHANNEL=beta` asks for the beta channel, and which the desktop app offers as an update only on
the beta channel.
