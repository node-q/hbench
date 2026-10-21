# ADR 0001: Datasets as OCI artifacts

**Status:** accepted · 2026-10-21

## Context

hbench runs a named dataset (`-d name@version`) from a registry: a JSON file that lists each
dataset's tasks, each task a git URL, a commit and a path (`src/registry.rs`). hbench inherited
the format and has no default registry of its own; users point it at one with `--registry-url`
or `--registry-path`. What that means in practice:

- **Nothing is content-addressed.** A task is pinned by a git commit (SHA-1), and the registry
  file by nothing: it is whatever its URL serves, fetched with `curl` on every run, with no cache
  or checksum. The most used registry is a 13.6 MB file on another organisation's default branch,
  listing 80 datasets and 51,625 tasks in 16 repositories.
- **Nothing is signed or verified before a run.** hbench computes two task hashes
  (`task_checksum` in `result.json`, the package digest in `lock.json`), but only after the
  task is on disk, against nothing. A cached task directory is trusted if it holds `task.toml`.
  A task changed upstream goes unnoticed until `run --diff`.
- **Environments drift.** Prebuilt task images are pinned by tag, or `:latest`, in personal or
  third-party Docker Hub accounts (Terminal-Bench 2.0: 89 images, 49 GB; SWE-bench Verified: 500
  images of about 1.1 GB). No hash covers them, and Docker Hub allows 100 anonymous pulls per six
  hours.
- **Mirrors and offline use are awkward.** A mirrored registry still points every task at
  GitHub; offline use needs a local registry file, a warm cache and pre-pulled images.
- **Licences are unstated** for most datasets in the large shared task repository, which has no
  LICENSE file.

hbench is a standalone tool. Its datasets should come from something any user can host, mirror
and verify, built on a standard that registries, clouds and tools already support.

## What the industry does

Benchmark *data* lives on Hugging Face, pinned to a commit (SWE-bench, LiveCodeBench,
HumanEvalPack; lm-evaluation-harness and inspect_evals load through it). Task *directories* live
in git, and their *environments* are Dockerfiles plus prebuilt OCI images per instance (SWE-bench
on Docker Hub, Epoch AI's and OpenHands' on GHCR), built ahead because images built at different
times differ. No framework yet ships task directories as OCI artifacts. The closest precedent is
ModelPack (CNCF), which packages model, dataset and code as OCI layers and which Docker Model
Runner produces.

OCI Image and Distribution 1.1 (2024) made registries general artifact stores (`artifactType`,
`subject`, the referrers API with a tag fallback). Helm, Flux, WebAssembly and Homebrew already
distribute through them. GHCR, Docker Hub, ECR, ACR, Google Artifact Registry, Quay, Zot and the
self-hosted registries all store OCI artifacts. Sigstore `cosign` signs them keylessly and stores
signatures as referrers, and GitHub artifact attestations add SLSA provenance. Croissant 1.1
(MLCommons, 2026) is the metadata standard for datasets, embedded by Hugging Face, Kaggle and
OpenML.

## Decision

1. **A dataset version is one OCI artifact.** One image manifest per `name@version`:
   - `artifactType: application/vnd.hbench.dataset.v1`;
   - a config blob `application/vnd.hbench.dataset.config.v1+json` (below);
   - one `application/vnd.oci.image.layer.v1.tar+gzip` layer per task, annotated with
     `org.opencontainers.image.title=<task>` and `dev.hbench.task.package-digest=sha256:…`;
   - manifest annotations `org.opencontainers.image.{version,licenses,source,revision,created}`.

   One manifest, not an index of per-task manifests, so that a whole dataset is one pull (Docker
   Hub counts every manifest as a pull). 500 tasks make about 175 KB, well under the 4 MB
   manifest guidance. A dataset over about 10,000 tasks is sharded into an index of manifests.

2. **Layers are reproducible.** A task is packed with sorted entries, fixed mtime, uid and gid,
   its exec bits kept, and `gzip -n`. Unpacked, it is byte-identical to the task directory it
   came from, so both task hashes are unchanged and an unchanged task keeps its layer digest from
   one dataset version to the next.

3. **The config blob is a small, versioned JSON Schema**, published with hbench:
   - per dataset: name, version (semver), description, SPDX licence, upstream
     `{url, revision, citation}`, canary string;
   - per task: name, layer digest, package digest, `docker_image` digest, platforms, timeouts.

4. **Addressing.**
   - `--registry oci://<registry>/<namespace>` with `-d terminal-bench@2.0` resolves
     `<registry>/<namespace>/terminal-bench:2.0` to its manifest digest.
   - `--registry oci-layout:<dir>` reads an OCI image layout on disk (offline, air-gapped).
   - Today's JSON keeps working through `--registry-url` and `--registry-path`.
   - hbench still has **no default registry**.
   - A job records the registry, repository, manifest digest and each task's layer digest in
     `config.json` and `lock.json`, alongside today's fields.
   - `hbench.lock` maps `name@version` to a manifest digest, and `--locked` refuses any drift.

5. **hbench verifies before it unpacks.** Every layer's digest is checked against the manifest,
   and the package digest against the config, before a task reaches its cache. The cache is
   content-addressed (`~/.cache/hbench/blobs/sha256/…`), so a cached task is trusted by digest,
   not by the presence of `task.toml`.

6. **Signatures and provenance.**
   - Datasets are published only by CI (GitHub Actions) from a public source repository.
   - They are signed with `cosign` keyless (GitHub OIDC) and attested with `actions/attest`;
     both are stored in the registry next to the dataset.
   - `--verify-signature` checks the publisher's workflow identity and the OIDC issuer. It is
     opt-in at first and the default one release later.
   - The result is recorded in the job manifest.

7. **Environments stay Dockerfiles; prebuilt images are pinned by digest.** A task that uses a
   prebuilt image sets `docker_image = "<image>@sha256:…"`, never a moving tag. Pinning changes
   the task's bytes, so it is published as a new dataset version, never by editing a published one.

8. **Metadata for discovery.** A Croissant 1.1 JSON-LD file, with RAI fields and
   `prov:wasDerivedFrom` naming the upstream dataset, is attached to each dataset as a referrer.
   hbench never needs it to run.

9. **node-q publishes its datasets** from the public repository `node-q/hbench-datasets`, on
   GitHub-hosted runners, to `ghcr.io/node-q/hbench-datasets/<dataset>`, mirrored to a second
   registry. A published tag is never moved; a new version is a new tag. Anyone can publish
   their own datasets the same way, to any OCI registry, with `hbench datasets push`.

## Alternatives considered

- **Hugging Face datasets as the registry.** That's where benchmark data already lives, and
  there are crates for it. But it is one vendor, it can't be hosted by the user, it has no
  signatures, its tags can be deleted and its history squashed, gated datasets need an account,
  and Docker task directories gain nothing from its dataset tooling. Kept as an optional
  mirror, and as the upstream a dataset's metadata names.
- **A static HTTPS index of content-addressed tarballs under TUF.** It has the strongest
  integrity model, but we would design and run the format, the client and the key ceremony
  ourselves, and no tool or registry speaks it. Docker retired its TUF-based Content Trust in
  2025 for lack of use.
- **Git repositories with tags (today's style).** It's easy to contribute to, but pins are SHA-1
  commits, tags move, clones of large task repositories are slow, nothing is signed, and there is
  no metadata standard.
- **One OCI artifact per task, under an index.** A task becomes a separate pull, which runs into
  Docker Hub's and ECR Public's pull limits for no gain, since reproducible layers already
  deduplicate unchanged tasks.

## Consequences

- **Any OCI registry works.** Datasets can be mirrored with `oras copy -r`, signatures included.
  Digests make a mirror trustless, and an OCI layout makes a dataset portable to a machine with no
  network.
- **A run can name exactly what it ran:** the manifest digest of its dataset and the layer digest
  of each task, verified before the run, not only hashed after it.
- **Consumers that pinned task hashes keep them**, as long as a dataset is converted without
  changing bytes, because layers unpack byte-identical. Pinning images by digest is a new dataset
  version.
- **hbench gains an OCI client** (`oci-client`, `oci-spec`, `ocidir`) and with it `reqwest`,
  where today it shells out to `curl` and `git`. Signature verification uses the `sigstore`
  crate, which is still marked experimental, or an installed `cosign`.
- **GHCR** has no referrers API (clients fall back to tags), no tag immutability and no
  published anonymous quota, and public storage is free only "currently". Hence a second
  registry from the start, and digests everywhere.
- **Republishing a dataset means redistributing it under its licence.** Each dataset carries its
  LICENSE and NOTICE and keeps its canary strings. A dataset whose licence does not allow
  redistribution is not republished; its metadata points at the upstream revision instead.
  Republishing prebuilt images also redistributes everything inside them.

## Rollout

1. **Read path.**
   - A dataset source abstraction over the JSON registry (unchanged), `oci://` and `oci-layout:`.
   - Digest checks before unpacking, and the content-addressed cache.
   - The new `lock.json` and `config.json` fields, `hbench.lock` and `--locked`.
   - **Done when** tests against an OCI layout fixture and a local registry show both task hashes
     byte-identical to the git source for the example tasks.
2. **Publisher tools.**
   - `hbench datasets pack`, `hbench datasets push`, and `hbench datasets convert <registry.json>
     --push oci://…`: fetch each git ref once, pack, push, and print `name@version` → digest.
   - `--check-equivalence` compares the git checkout with the extracted copy file by file.
   - `hbench datasets export --oci-layout <dir>`.
   - **Done when** converting a whole dataset reproduces every `task_checksum` and package digest.
3. **Publishing repository.** `node-q/hbench-datasets`:
   - sources or conversion configs, with each dataset's LICENSE and NOTICE;
   - a schema check in CI;
   - a publish workflow on GitHub-hosted runners: pack, push to GHCR and the mirror, sign, attest,
     attach Croissant.
   - **Done when** the first dataset is public and passes `cosign verify` and
     `gh attestation verify`.
4. **Environments.**
   - A new version of each dataset whose tasks use prebuilt images pins them by digest.
   - Images are mirrored only where their licences allow.
   - **Done when** no published task depends on a moving tag.
5. **Verification.**
   - `--verify-signature` with an identity policy, recorded in the job manifest.
   - **Done when** it's on by default one release after it ships.
6. **Deprecation.**
   - The JSON registry is read for at least two more minor releases, and `--registry-url` warns.
   - The README documents the OCI path first.
   - The JSON reader stays for local files.

## Open questions

1. **Which datasets node-q republishes.** Each needs a licence check first: Terminal-Bench 2.0
   (Apache-2.0) and HumanEvalPack (MIT) are clear. Aider Polyglot's exercises come from Exercism
   (MIT) but its repository has no licence file. SWE-bench's tasks are MIT, but its images embed
   GPL-2.0 code among others. LiveCodeBench's licence is unspecified and its problems come from
   contest sites.
2. **Which registry mirrors GHCR**: Quay.io, ECR Public, or Docker Hub (whose referrers support
   is unconfirmed).
3. **Prebuilt images**: pin the upstream images by digest only, or also mirror them.
4. **A Hugging Face mirror**, for discovery.
5. **When signature verification becomes the default**, and the identity policy for datasets
   published by others.
