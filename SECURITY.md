# Security

## Reporting a vulnerability

Open an issue on node-q/hbench titled "Security report" that says only that you have
one, with no details, and a maintainer replies with a private way to send it. Please
keep the details out of public issues. Include, once asked: what you found, how to
reproduce it, and the hbench version (`hbench --version`) and platform.

Fixes go into the next release; there are no backports to older ones. The newest release
is on this repository's Releases page, and `scripts/install.sh` installs it.

## Checking the installer

The script checks the archive it downloads against the release's `SHA256SUMS`. To check the script itself
before running it, download it, read it, and verify that this repository's release workflow built it (releases
after v0.3.7; `gh attestation verify` needs the [GitHub CLI](https://cli.github.com)):

```bash
curl -fsSLO https://github.com/node-q/hbench/releases/latest/download/install.sh
curl -fsSLO https://github.com/node-q/hbench/releases/latest/download/SHA256SUMS
grep ' install.sh$' SHA256SUMS | sha256sum -c    # macOS: shasum -a 256 -c
gh attestation verify install.sh --repo node-q/hbench
less install.sh
sh install.sh
```

The release archives verify the same way: `gh attestation verify hbench-<version>-<target>.tar.gz --repo node-q/hbench`.

## What hbench does and does not protect

hbench runs tasks and agents in Docker containers on your machine. A container shares
the host's kernel and is not a boundary against hostile code: run tasks and agents you
trust, or put hbench in a VM you can throw away.

- **Egress.** With `enforce_allowlist`, the agent phase reaches only the allowed hosts
  (docs/REFERENCE.md, "Task format"), but DNS to the configured resolvers and ICMP stay open, and setup
  and the verifier keep the task's own network.
- **Secrets.** Values of environment variables named like keys are redacted from a trial's
  files, raw and in the common encodings. A secret an agent hashes, encrypts or splits is
  not recognised.
- **Resources.** Tasks get the CPU and memory limits they declare; disk is not limited.
- **The viewer.** `hbench view` binds 127.0.0.1. Bound to any other address it serves
  read-only: the routes that start runs and delete jobs answer 403 there.

Reports about any of these are welcome; they are the known limits, not the whole list.
