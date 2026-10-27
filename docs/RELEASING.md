# Releasing

Bump `version` in `Cargo.toml` in a pull request, merge it, then push a matching tag on the merge commit:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` checks the tag against `Cargo.toml` and, on GitHub-hosted runners, builds static
x86_64/aarch64 Linux (musl) and x86_64/arm64 macOS binaries, one per job. It publishes only once `ci.yml` has passed
on the tagged commit's push to `master` (unit tests on Linux and macOS, E2E, lint, audit), waiting for that run if it
is still going, so a tag on a commit CI never ran on, or one it failed, publishes nothing. Each `.tar.gz` gets a build provenance
attestation; check one with `gh attestation verify hbench-v0.2.0-<target>.tar.gz -R node-q/hbench`. A separate publish
job, in the `release` environment, creates the GitHub release with the archives, the tagged commit's `scripts/install.sh`
(so `releases/latest/download/install.sh` is always the latest release's), `SHA256SUMS` and generated notes.
`SHA256SUMS` lists the archives and `install.sh`, and the publish job attests `install.sh` and `SHA256SUMS` too, so
`gh attestation verify install.sh -R node-q/hbench` checks the script and `sha256sum -c` checks any file against the
sums. The install script checks the archive it downloads against `SHA256SUMS` before unpacking it. Tags
with a suffix (`v0.2.0-rc.1`) are published as prereleases, which the install script skips unless `HBENCH_VERSION`
names one.
