# Security

## Reporting a vulnerability

Email hello@nodeq.tech with "hbench security" in the subject, and please don't open a
public issue for it. Say what you found, how to reproduce it, and which hbench version
(`hbench --version`) and platform you used.

Fixes go into the next release; there are no backports to older ones. Install the latest
with `curl -fsSL https://nodeq.tech/install.sh | sh`.

## What hbench does and does not protect

hbench runs tasks and agents in Docker containers on your machine. A container shares
the host's kernel and is not a boundary against hostile code: run tasks and agents you
trust, or put hbench in a VM you can throw away.

- **Egress.** With `enforce_allowlist`, the agent phase reaches only the allowed hosts
  (README, "Task format"), but DNS to the configured resolvers and ICMP stay open, and setup
  and the verifier keep the task's own network.
- **Secrets.** Values of environment variables named like keys are redacted from a trial's
  files, raw and in the common encodings. A secret an agent hashes, encrypts or splits is
  not recognised.
- **Resources.** Tasks get the CPU and memory limits they declare; disk is not limited.
- **The viewer.** `hbench view` binds 127.0.0.1. Binding another address exposes its
  routes, including the ones that start runs and delete jobs, to that network.

Reports about any of these are welcome; they are the known limits, not the whole list.
