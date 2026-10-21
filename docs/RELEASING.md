# Releasing

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` checks the tag against `Cargo.toml` and, on GitHub-hosted runners, runs the tests and
builds static x86_64/aarch64 Linux (musl) and x86_64/arm64 macOS binaries. Each `.tar.gz` gets a build provenance
attestation; check one with `gh attestation verify hbench-v0.2.0-<target>.tar.gz -R node-q/hbench`. A separate publish
job, in the `release` environment, creates the GitHub release with the archives, `SHA256SUMS` and generated notes. Tags
with a suffix (`v0.2.0-rc.1`) are published as prereleases, which the install script skips unless `HBENCH_VERSION`
names one.
