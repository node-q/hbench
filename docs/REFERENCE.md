# hbench reference

The details behind the [README](../README.md): datasets and registries, agents and their credentials, the task
and dataset formats, the output, troubleshooting and development.

## Running

A named dataset (`-d name@version`) comes from the default dataset registry, `oci://ghcr.io/node-q/hbench-datasets`,
unless `--registry` names another OCI registry or OCI image layouts (see
[Datasets from an OCI registry](#datasets-from-an-oci-registry)), or `--registry-url` / `--registry-path` a
`registry.json` (in a job config, the dataset's `registry`, `registry_url` or `registry_path`). `HBENCH_REGISTRY`
replaces the default, in the same forms as `--registry`. Local task directories (`-p`) need no registry.

Docker: hbench runs every trial in Linux containers on whatever `docker` points at (`DOCKER_CONTEXT`/`DOCKER_HOST`).
Remote ssh daemons work because files move with `docker compose cp` rather than bind mounts.

`run`, `jobs resume`, `trials start` and `sweeps run` exit 130 when cancelled (SIGINT, SIGTERM or SIGHUP; a second
signal force-quits after removing the run's compose projects), 1 on an error or when every trial errored
(`--fail-on-error`: any trial), and 0 otherwise. Before starting they check agent kwargs against the agent's schema and,
for `claude-code`, `codex` and `gemini-cli`, that some credential is set; `--skip-preflight` skips the credential check.
For `sweeps run`, every trial means every trial of all its sweeps.

## Datasets

`hbench datasets list` lists the datasets of the default registry (`--registry`, `--registry-url` or
`--registry-path` lists another's):

```bash
hbench datasets list
```

Run one:

```bash
hbench run -d terminal-bench@2.0.1 -a claude-code -m anthropic/claude-opus-5-5 -n 4
```

A `registry.json` lists datasets whose tasks live in git repositories:

```bash
hbench datasets list --registry-url <registry-url>
hbench run -d <name>@<version> --registry-url <registry-url> -a claude-code -m anthropic/claude-opus-5-5
```

## Agents

46 agents (`hbench agents list`): `oracle`, `nop`, `terminus-2` (also `terminus`, `terminus-1`), `claude-code`, `codex`,
`gemini-cli`, `opencode`, `aider`, `goose`, `copilot-cli`, `cursor-cli`, `openhands`, `openhands-sdk`, `mini-swe-agent`,
`swe-agent`, `trae-agent`, `qwen-coder`, `kimi-cli`, `kimi-code`, `grok-build`, `hermes`, `junie`, `rovodev-cli`,
`cline-cli`, `cortex-code`, `code-puppy`, `devin`, `langgraph`, `deerflow`, `strands`, `nemo-agent`, `mimo`, `mcode`,
`pi`, `openclaw`, `antigravity-cli`, `antigravity-sdk`, `eve`, `fx`, `fx-dev`, `vibe`, `muse-code`, `acp` (+ `acp:<id>`),
`computer-1`, `dspy-rlm`, `deepseek-harness`.

Each agent has pinned install commands and versions, validated CLI flags and env (`--ak key=value` / `kwargs`), auth
resolution, error classification (`ApiRateLimitError`, `AgentAuthenticationError`, ...), and usage/cost parsing. Cost
uses a pinned LiteLLM price table.

Installs default to the versions in `src/agents/installed/pins.rs`, helpers included (uv, nvm, bun), so an upstream
release changes neither results nor installs until the pin moves. `--ak version=X` installs another version and
`version=latest` the newest. The vendor installers of `antigravity-cli`, `cortex-code`, `cursor-cli`, `junie`, `mimo`,
`muse-code` and `rovodev-cli` take no version and always fetch the newest release; `result.json` records what was
installed. `scripts/dev/check-pins.py` lists the pins with a newer release.

The most used ones:

| Agent | What it runs | Model (`-m`) | Useful options (`--ak`) |
|---|---|---|---|
| `claude-code` | Anthropic's Claude Code; an API key or a subscription token (`claude setup-token`) | `anthropic/claude-opus-5-5` | `reasoning_effort`, `max_turns`, `max_budget_usd` |
| `codex` | OpenAI's Codex CLI | `openai/gpt-6.1-sol` | `reasoning_effort`, `web_search` |
| `gemini-cli` | Google's Gemini CLI | `google/gemini-3-pro` | `reasoning_effort`, `sandbox` |
| `opencode` | opencode, with any provider's model | `<provider>/<model>` | `variant` |
| `terminus-2` | A minimal tmux-based agent, with any provider's model | `<provider>/<model>` | `max_turns`, `temperature` |
| `deepseek-harness` | DeepSeek's own harness ([`dsh`](https://github.com/deepseek-ai/deepseek-harness)) | `deepseek/deepseek-v4-flash` | `reasoning_effort` |
| `oracle` | Each task's reference solution, to check the tasks | — | — |

`hbench agents schema <agent>` lists every option of an agent, with its type and default.

### Cost limit

`--max-cost-usd 2` (`agent.max_cost_usd` in a job config) caps what the agent may spend in a trial, in US dollars, as
hbench counts its cost (`cost_usd` in `result.json`). The limit is recorded with the agent's config in the trial's
`config.json`, `lock.json` and `result.json`. A trial over it ends with `AgentCostLimitError`, unless the run had
already failed for another reason: as after a timeout, the verifier still grades what the agent left, and the trial is
not retried by default. In a multi-step task the
limit covers all steps together: each step may spend what the earlier ones left, and no step runs after the one that
reaches it.

Whether hbench can stop the agent at the limit depends on whether it sees the spend while the agent runs:

- `terminus-2` and `computer-1` (with its default chat-completions provider) call the model from hbench and price
  every call. They check the budget before each turn: the turn that reaches the limit completes, and the agent stops
  before its next model call. A trial can therefore go over the limit by one turn's calls, a summarization included.
- Every other agent runs its own CLI or SDK in the container, and hbench reads its cost from its logs once it exits.
  For these agents the limit is checked after the run: the trial still ends with `AgentCostLimitError`, but the money
  is already spent, and only the agent timeout bounds the run. Some agents have a cap of their own that does stop
  them, such as `claude-code`'s `--ak max_budget_usd=…`.
- A run that reports no cost (a model missing from the price table, `computer-1`'s native providers, an agent whose
  logs give no cost) cannot be checked at all; `trial.log` says so.

`max_cost_usd` cannot be combined with `user_agent`.

### Environment variables

Credentials are read from the host environment or `--ae KEY=VALUE` (`agent.env` in a job config).

| Agent | Credentials (any one) | Optional |
|---|---|---|
| `claude-code` | `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`); `CLAUDE_CODE_OAUTH_TOKEN` (`claude setup-token`; `CLAUDE_FORCE_OAUTH=1` prefers it); Bedrock: `AWS_BEARER_TOKEN_BEDROCK` or `CLAUDE_CODE_USE_BEDROCK=1` + AWS credentials. `ANTHROPIC_BASE_URL` alone (a proxy that holds the key) and `CLAUDE_CODE_USE_VERTEX=1` also pass the credential check; hbench forwards no Vertex settings, so give those with `--ae` | `ANTHROPIC_BASE_URL` |
| `codex` | `OPENAI_API_KEY`; `CODEX_AUTH_JSON_PATH=<auth.json>` or `CODEX_FORCE_AUTH_JSON=1` (`~/.codex/auth.json`) | `OPENAI_BASE_URL` |
| `gemini-cli` | `GEMINI_API_KEY` (or `GOOGLE_API_KEY`); `GEMINI_OAUTH_CREDS_PATH` or `GEMINI_FORCE_OAUTH=1` (`~/.gemini/oauth_creds.json`); Vertex: `GOOGLE_GENAI_USE_VERTEXAI=1` + `GOOGLE_CLOUD_PROJECT`/`GOOGLE_CLOUD_LOCATION` | `GOOGLE_GEMINI_BASE_URL` |
| `deepseek-harness` | `DEEPSEEK_API_KEY` | `DEEPSEEK_BASE_URL` |
| model-routed agents (`terminus-2`, `aider`, `opencode`, `goose`, `mini-swe-agent`, `swe-agent`, ...) | the key of the `-m provider/model` prefix: `anthropic/` `ANTHROPIC_API_KEY`, `openai/` `OPENAI_API_KEY`, `gemini/` `GEMINI_API_KEY`, `openrouter/` `OPENROUTER_API_KEY`, `deepseek/` `DEEPSEEK_API_KEY`, `xai/` `XAI_API_KEY`, `bedrock/` `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` + `AWS_REGION` | `<PROVIDER>_BASE_URL` |
| `analyze`, `check` | run `claude-code` by default (`-a` picks another agent), so they need an Anthropic credential, usually `ANTHROPIC_API_KEY` | |

`hbench agents schema <agent>` lists an agent's kwargs; `src/agents/model_connection.rs` has every provider's key names.
With a ChatGPT login (`CODEX_AUTH_JSON_PATH`, `CODEX_FORCE_AUTH_JSON`), codex takes only the model names Codex lists, such as
`gpt-6.1-sol` or `gpt-5.6-sol`; an API alias such as `gpt-5.6` fails there with HTTP 400.
Other hbench variables: `DOCKER_CONTEXT`/`DOCKER_HOST` (which daemon), `HBENCH_EGRESS_METHOD=proxy` (see below).
`HBENCH_NO_UPDATE_CHECK=1` stops the daily check for a newer release, which suggests `hbench update` (also off when `CI` is set or stderr is not a
terminal); hbench reads the last result from `~/.cache/hbench/latest-release.json` and refreshes it in the background.

## Task format

A task is a directory:

```
task/
├── task.toml        # [task] [agent] [verifier] [environment] [[steps]] artifacts=[...] ...
├── instruction.md   # or steps/<name>/instruction.md for multi-step tasks
├── environment/     # Dockerfile and/or docker-compose.yaml (`main` = agent container)
├── tests/test.sh    # writes /logs/verifier/reward.txt or reward.json
└── solution/solve.sh
```

Task features:

- Multi-step tasks: per-step workdir/setup.sh, healthcheck, `min_reward` gates, mean/final reward strategy, and resume.
- Shared and separate verifier environments, including bundled verifier images.
- Artifacts: sidecar sources, destinations, excludes, `manifest.json`, and `[[verifier.collect]]` hooks.
- Network policies: `public`, `no-network`, and `allowlist`, phase-scoped per agent/verifier/step. An egress
  sidecar enforces them, and run-level `extra_allowed_hosts` are supported. Those hosts only
  widen a restricted task and are ignored on a public one (most registry tasks declare no policy, so they are
  public); set `agent.enforce_allowlist: true` to make them the agent phase's whole allowlist there too.
  How the sidecar enforces them is recorded per trial in `result.json` as `egress` (see below).
- Healthchecks, `workdir`, task env, agent/verifier users, resource limits, and GPUs.
- MCP servers and skills.
- `trajectory.json` / `load_trajectory` (native + ATIF), the simulated-user ACP bridge (`user_agent`), regrades, and
  diffs.

The egress sidecar runs gost with the allowlist and sets `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` (both cases) in
the task container, so proxy-aware clients send it host names and need no DNS of their own. It enforces the
allowlist in one of two ways, and `result.json` records which, with the refused destinations:
`"egress": {"enforced": true, "method": "nft+proxy", "denied": ["example.com:443"]}`.

- `nft+proxy`, wherever the daemon's kernel takes the sidecar's nftables rules (Linux and Docker Desktop on
  macOS): the task container shares the sidecar's network namespace, all of its TCP goes through gost, and other
  protocols are refused.
- `proxy`, where it does not: the task container sits on an internal network with no route out, and the proxy is
  its only way out. TCP that skips the proxy fails even to an allowed host, so a task that needs it fails rather than runs
  open. Public phases attach the container to the outside network again. `HBENCH_EGRESS_METHOD=proxy` forces
  this method.

If neither can be set up, or the task's main service picks its own network, the trial fails at setup.

hbench rejects keys it does not know in a job config or `task.toml`, so a misspelt `enforce_allowlist` stops the
run instead of leaving the agent's network open. Legacy spellings are rewritten first:
`orchestrator.{n_concurrent_trials,quiet,retry}` move to the top level, a dataset's
`registry: {url, path}` becomes `registry_url`/`registry_path`, `environment.mounts_json` becomes `mounts`,
`plugins` and `environment.suppress_override_warnings` (both no-ops) are dropped, and a top-level `version` in
`task.toml` means `schema_version`. Recorded `config.json` and `result.json` files are still read as they are.

Some registry datasets build verifier-only data into the agent's image: LiveCodeBench's `/app/config.json` holds
the hidden tests, and Aider Polyglot and TB2's adaptive-rejection-sampler ship the reference solution encrypted with
the password in their public `solve.sh`. hbench removes these from the container before any agent but the oracle
starts (`src/integrity.rs`); the verifier still gets its own complete `tests/`.

When the agent phase ends, by timeout or a normal exit, hbench stops the agent's own processes before grading: those
still running under the agent's `docker compose exec`, and the foreground job of its terminal. Processes the agent
detached on purpose, such as a server a task asks it to leave running (started with `nohup`, `setsid`, `&`, `tmux`
or a service manager), keep running for the verifier. If they cannot be stopped, the trial ends
ungraded with the run's own error (`AgentTimeoutError` after a timeout). In a shared verifier environment such a process
could also change files while the tests run; it shows up in the trajectory, so review trajectories of top runs.

## Datasets from an OCI registry

`--registry` reads datasets from an OCI registry, or from OCI image layouts on disk, instead of a `registry.json`:

```bash
hbench run -d hello@1.0 --registry oci://ghcr.io/<namespace> -a oracle
hbench datasets download hello@1.0 --registry oci-layout:./datasets    # ./datasets/hello is an OCI image layout
hbench datasets list --registry oci-layout:./datasets
hbench datasets list --registry oci://ghcr.io/<namespace>              # what its dataset index lists
```

- `oci://<registry>/<namespace>`: `-d <name>@<version>` is the manifest tagged `<version>` in
  `<registry>/<namespace>/<name>`. Pulls are anonymous (with a registry token where the registry asks for one, as
  GHCR does) and over HTTPS, except to `localhost`, `127.0.0.1` and `::1`, which use plain HTTP.
- `oci-layout:<dir>`: `<dir>/<name>` is an OCI image layout, and the dataset is the manifest in its `index.json`
  whose `org.opencontainers.image.ref.name` is `<version>`. `oras cp <registry>/<namespace>/<name>:<version>
  --to-oci-layout <dir>/<name>:<version>` makes one, for offline use.
- `datasets list --registry oci://<registry>/<namespace>` lists the datasets the namespace's
  [dataset index](#dataset-index) names: an OCI registry has no list of its repositories that hbench can read
  (GHCR serves none), so without an index a dataset is run by name. An `oci-layout:<dir>` is listed from its
  subdirectories.
- In a job config, a dataset sets `registry: "oci://..."` or `registry: "oci-layout:..."` (a string; the object
  `registry: {url, path}` is still the legacy `registry.json` form) with a `name` and a `version`, and may set
  `digest: "sha256:..."` to run that manifest whatever the tag says now.

Nothing reaches the task cache unchecked. The manifest must match its digest, the config blob must match
[`assets/schemas/dataset-config.v1.json`](../assets/schemas/dataset-config.v1.json) and the manifest and name the dataset
`-d` asks for, and each layer must match its digest and size, both while it downloads and while it unpacks. The unpacked files must match the task's
package digest. Absolute paths, `..`, hard links, device files and symlinks that leave the task directory are
refused. A layer is unpacked into a temp directory and renamed into place. Blobs are cached in
`~/.cache/hbench/blobs/sha256/<hex>`, and tasks in `~/.cache/hbench/tasks/sha256/<layer hex>/<task>` (both under
`<dir>` with `datasets download -o <dir>`). A cached task is used only while it matches its package digest, and is
unpacked again otherwise.

A job records what it ran: the job's `config.json` has each dataset's `registry` and manifest `digest`, and each
trial's `lock.json` task has `type: "oci"`, `registry`, `repository`, `manifest_digest` and `layer_digest`. Unpacked,
a task is byte-identical to the directory it was packed from, so `task_checksum` (`result.json`) and the package
digest (`lock.json`'s `task.digest`) are the ones `hbench tasks digest -p <task dir>` prints for that directory.

`--lock-write` records each OCI dataset's manifest digest in `./hbench.lock`, keyed by `name@version` (not by
registry, so a mirror must serve the same digest). A dataset in `hbench.lock` runs at its locked digest, with a
warning if its tag has moved since; `--locked` refuses a dataset that is not in `hbench.lock`, or whose tag now
resolves to another digest. `run` and `datasets download` take both flags.

### Publishing datasets

```bash
hbench datasets pack tasks --descriptor source.yaml --oci-layout ./datasets      # ./datasets/<name>, tagged <version>
hbench datasets push tasks --descriptor source.yaml --to oci://ghcr.io/<namespace>/<name>:<version>
hbench datasets convert registry.json -d hello@1.0 --oci-layout ./datasets --check-equivalence
hbench datasets export -d hello@1.0 --registry oci://ghcr.io/<namespace> --oci-layout ./datasets
```

- `pack` packs every subdirectory of `tasks` that has a `task.toml`, sorted by name, and skips everything else. Its
  layers are reproducible, so the same tasks and descriptor always make the same manifest. `pack`, `push` and
  `export` print the manifest digest on stdout, and a summary on stderr.
- The descriptor is YAML matching [`assets/schemas/dataset-source.v1.json`](../assets/schemas/dataset-source.v1.json),
  which needs only `schema_version: 1`, `name`, `version` and `license`, and accepts node-q/hbench-datasets'
  `source.yaml` files as they are. `description`, `license`, `upstream` (`url`, `revision`, `creator`, `citation`) and
  `canary` go into the config blob. `title` and
  `description` become the manifest's `org.opencontainers.image.title` and `.description`, and `source.git_url` and
  `source.commit` its `.source` and `.revision` (`upstream.url` and `.revision` without a `source`). Packing fails if
  the number of tasks is not `source.task_count`, or a task's `docker_image` is not `environment.declared_image`
  (`{task}` being the task's directory name) or, with `policy: pinned-by-digest`, its entry in `environment.images`.
  `source.path`, `keywords`, `attribution`, `rai` and `environment.planned` are checked but not used.
- `push` takes `--to oci://<registry>/<repository>:<version>`, the repository ending in the dataset name. It uploads the
  blobs the registry lacks, then the manifest, and checks that the tag resolves to the manifest's digest. Pushing the
  same manifest again changes nothing; a tag that names another manifest is moved only with `--allow-overwrite`.
  Credentials come from the docker config that `docker login` and `oras login` write (`$DOCKER_CONFIG/config.json`, else
  `~/.docker/config.json`): the registry's credential helper (`credHelpers`, else `credsStore`), else its `auths` entry.
  Without any, `push` is anonymous. As for pulls, only loopback registries are reached over plain HTTP.
- `convert` finds `name@version` in a `registry.json` (a path or a URL), checks out each git repository and commit
  once, and packs the tasks into `--oci-layout` or pushes them `--to` a registry. It prints `name@version` and the
  manifest digest. Without `--descriptor` the dataset takes the registry's name, version and description, licence
  `NOASSERTION`, and the git URL and commit its tasks share as `.source` and `.revision`. `--check-equivalence` reads the
  result back, unpacks every task and compares it with its checkout: the same files, contents, exec bits and symlink
  targets, `task_checksum` and package digest. Any difference fails the command.
- `export` copies `name@version` from `--registry` (as `run` takes it) into `<dir>/<name>`, tagged with the version and
  checked blob by blob, so that `--registry oci-layout:<dir>` runs it offline. It reads with the docker config's
  credentials; `run` and `datasets download` pull anonymously.

### Dataset format

A dataset version is one OCI image manifest (`application/vnd.oci.image.manifest.v1+json`), tagged with the version:

- `artifactType`: `application/vnd.hbench.dataset.v1`.
- `config`: a JSON blob of media type `application/vnd.hbench.dataset.config.v1+json`, matching
  [`assets/schemas/dataset-config.v1.json`](../assets/schemas/dataset-config.v1.json): the dataset's `name` (the last
  component of its repository), `version` (semver without build metadata, the patch number optional: `2.0`), SPDX
  `license`, and optionally `description`, `upstream` (`url`, `revision`, `creator`, `citation`) and `canary`; and
  `tasks`, one per layer and in layer order, each with `name`, `layer_digest` and `package_digest`, and optionally
  `docker_image` (pinned, `<image>@sha256:<hex>`), `platforms` (`os/arch`) and `timeouts` (`agent_sec`,
  `verifier_sec`, `build_sec`).
- `layers`: one `application/vnd.oci.image.layer.v1.tar+gzip` per task, annotated with
  `org.opencontainers.image.title` (the task's directory name, which equals its config `name`) and
  `dev.hbench.task.package-digest` (its config `package_digest`).
- Manifest annotations `org.opencontainers.image.version` (equal to the config's version when set), `.licenses`,
  `.title`, `.description`, `.source`, `.revision` and `.created`.

A layer is a tar of the task directory's contents, paths relative to it (`./` prefixes are fine): regular files,
directories, and symlinks that resolve inside the task directory. Files with an exec bit unpack as 0755, other files
as 0644, directories as 0755; owners and times are not restored. The package digest is what
`hbench tasks digest -p <task dir>` prints as `digest`: `sha256:` and the SHA-256 of `<path>\0<sha256 hex of the
file>\n` for each file the task packages (`task.toml`, `instruction.md`, `README.md`, `trajectory.json`, and
`environment/`, `tests/`, `solution/` and `steps/`, less what the task's `.gitignore` ignores, or without one
`__pycache__/`, `*.pyc`, `.DS_Store`, `*.swp`, `*.swo` and `*~`), sorted by path.

Layers should be reproducible, so that an unchanged task keeps its layer digest from one dataset version to the next:
entries sorted by path, modes 0755 (directories and executables) or 0644, uid and gid 0, mtime 0, and gzip without a
file name or time. `hbench datasets pack` makes them so; with GNU tar and [oras](https://oras.land) instead,
publishing a one-task dataset looks like this:

```bash
tar -C tasks/hello-world --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --mode=a=rX,u+w -cf - . \
  | gzip -n > hello-world                                # the file name becomes the layer's title
cat > annotations.json <<'JSON'
{"$manifest": {"org.opencontainers.image.version": "1.0", "org.opencontainers.image.licenses": "Apache-2.0"},
 "hello-world": {"dev.hbench.task.package-digest": "<digest from hbench tasks digest -p tasks/hello-world>"}}
JSON
cat > config.json <<'JSON'
{"name": "hello", "version": "1.0", "license": "Apache-2.0",
 "tasks": [{"name": "hello-world", "layer_digest": "sha256:<sha256sum hello-world>",
            "package_digest": "<digest from hbench tasks digest -p tasks/hello-world>"}]}
JSON
oras push ghcr.io/<namespace>/hello:1.0 --artifact-type application/vnd.hbench.dataset.v1 \
  --config config.json:application/vnd.hbench.dataset.config.v1+json --annotation-file annotations.json \
  hello-world:application/vnd.oci.image.layer.v1.tar+gzip
```

### Dataset index

A registry lists its datasets in `<namespace>/index`: the manifest tagged `latest` there, an OCI image manifest with
`artifactType` `application/vnd.hbench.dataset-index.v1`, an empty config (`application/vnd.oci.empty.v1+json`) and one
layer of media type `application/vnd.hbench.dataset-index.v1+json`, matching
[`assets/schemas/dataset-index.v1.json`](../assets/schemas/dataset-index.v1.json):

```json
{"schema_version": 1,
 "datasets": [{"name": "hello", "version": "1.0", "digest": "sha256:<manifest digest of hello:1.0>"}]}
```

`datasets list` and the interactive `hbench run` read it. hbench opens every listed dataset by its digest, with the
checks any dataset manifest and config pass, and refuses an entry whose dataset names another `name@version`, so the
index only says which datasets there are. Since it lives at `<namespace>/index`, no dataset can be named `index`. oras
1.1 and later push an index with the empty config on their own:

```bash
oras push ghcr.io/<namespace>/index:latest --artifact-type application/vnd.hbench.dataset-index.v1 \
  index.json:application/vnd.hbench.dataset-index.v1+json
```

## Output

```
jobs/<job>/{config.json, lock.json, result.json, job.log, trials.json, manifest.json}
jobs/<job>/<task>__<id>/{config.json, lock.json, result.json, trial.log, exception.txt,
                         agent/, verifier/, artifacts/{manifest.json, logs/artifacts/...}, steps/<name>/...}
```

`trials.json` is written only for jobs not planned from their config (`jobs regrade`, `run --diff`), so they can be
resumed. The job's `result.json` has per-eval metrics (mean/sum/min/max/uv-script), pass@k, reward/exception stats, tokens,
and cost. Failed trials are retried by error type (`--max-retries`, include/exclude).

`lock.json` (the job's and each trial's) records the build that ran it: its version, its results epoch and the
commit it was built from. Releases of one results epoch grade alike, so `run --diff` reuses their trials whatever
the version. `manifest.json`, written last, holds the SHA-256 of the job's config, lock, result and log and of every
file its trials wrote; `hbench jobs verify -p jobs/<job>` reports any file changed, removed or added since. It shows
the files are the ones this build wrote, not that the run was real: anyone can rebuild hbench.

Secrets stay out of the output and off command lines:

- hbench treats as secret the value of every environment variable whose name looks like one (`*_KEY`, `*TOKEN`,
  `*SECRET*`, `*PASSWORD*`, `*CREDENTIAL*`, `*AUTH*`), from the host, the agent's `env`, agent kwargs and a task's
  `env` tables. Values shorter than 8 characters are not treated as secrets.
- When a trial ends, these values are replaced with `[REDACTED]` in every file of the trial directory: trajectories,
  agent and verifier logs, artifacts and `result.json`. The job directory gets the same treatment when the job ends,
  and `trial.log` is scrubbed as it is written. Each value is matched raw, JSON-escaped, URL-encoded and in base64,
  including inside a longer base64 string such as an `Authorization: Basic` header.
- Commands and their environment reach the container on `docker compose exec`'s stdin, so `ps` and `/proc` show neither,
  on the host or in the container.

Agent setup is hardened so environment problems aren't charged to the agent:

- A failed agent install is retried (3 tries within the setup timeout).
- If it still fails, it's recorded as `AgentSetupError` ("failed before the agent started") rather than as an agent
  failure.
- apt installs fall back to the image's sources minus the `*-security` suites. This matters for EOL releases like
  Debian bullseye, whose security index lists packages that are gone from the mirror.

### Uploading a job

`hbench jobs upload` sends a finished job to a server that takes one: the job folder as a gzipped tar (the same
archive `tar czf job.tgz -C jobs <job>` makes) in one POST. It checks the job against its `manifest.json` first and
sends nothing if a file changed. `--token-env` names an environment variable whose value goes out as
`Authorization: Bearer …`, so the token stays off the command line; `-H` adds any other header. It prints the
server's answer and exits 1 with the server's reason if it refused the job.

```bash
export UPLOAD_TOKEN=…
hbench jobs upload -p jobs/<job> --url https://example.com/api/uploads --token-env UPLOAD_TOKEN -H 'run-token: …'
```

## Troubleshooting

- **Watching a run**: `-o`/`--observe` (on `run` and `jobs resume`) prints every trial's `trial.log` lines and
  each command's output as it happens, every line under the trial's name, redacted like `trial.log`. A command
  shows its first line, and a nonzero exit its code. The oracle's and the verifier's output, which go to files in the
  container, show their last 200 lines when they finish. With several trials at once their lines interleave, so
  `-n 1` reads best. The live status line is off while observing.
- **Docker unreachable** (`docker version` timed out / daemon unreachable): hbench talks to whatever `docker` uses.
  Check `docker context ls`, `DOCKER_CONTEXT` and `DOCKER_HOST`; for an ssh context, make sure the remote daemon is up
  and an existing ssh connection isn't stale.
- **Leaked containers** (after a force quit or a crash): trial compose projects are named after the trial
  (`<task>__<id>`). List them with `docker compose ls -a` and remove one with
  `docker compose -p <name> down -v -t 0`; `hbench cache clean` removes hbench-built images and the cache dir.
- **Rate limits**: trials that hit them end with `ApiRateLimitError`. Lower `-n`/`--n-concurrent-agent`, then
  `hbench jobs resume -p jobs/<job> -f ApiRateLimitError` reruns just those trials (or retry them in the run with
  `--max-retries N --retry-include ApiRateLimitError`).
- **Missing credentials**: the run stops before starting trials and names the variable to set (see
  [Environment variables](#environment-variables)).

## Development

### Viewer

`hbench view` serves its own web UI (`viewer/`, React + Vite) for browsing jobs and tasks. It listens on 127.0.0.1 by
default; with `--host` set to any non-loopback address it is read-only, because starting runs and deleting jobs would be
open to anyone who can reach it. The built bundle (`viewer/dist`) is committed and embedded at compile time, so
`cargo build` doesn't need Node. After changing the UI:

```bash
cd viewer && npm install && npm run build      # type-check + rebuild viewer/dist
npm run dev                                    # hot reload; proxies /api to `hbench view <folder> --port 8091`
```

### Verification

- `cargo test`: unit tests for every module and agent.
- `scripts/e2e.sh`: a fixture suite against a real docker daemon. It covers timeouts, missing rewards, reward.json,
  users/env, no-network, compose sidecars, prebuilt images, egress allowlists, and secrets (kept off command lines
  and scrubbed from the job). It then runs every other command of the CLI against the job dirs those runs left, the
  example tasks, and inputs it makes on the spot: `jobs` (summary, verify, resume, regrade, upload to a stand-in
  server), `trials`, `traces`, `datasets` (an OCI layout, a `registry:3` container it starts, a registry.json over a
  local git checkout), the authoring commands, `tasks migrate` on a Terminal-Bench v1 task that is then run, `sweeps`,
  `exec`, `analyze` and `check` with the nop agent, `tasks start-env`, `cache clean --dry`, and the viewer's API.
  Last, it checks for leaked containers. Not covered: `hbench update` (it would install a release over the binary
  under test) and the interactive paths (the `run` wizard, a real `trials handoff`, the `start-env` shell).
- `scripts/install-matrix.sh`: installs every installed agent into a real container (`install_only`).

CI (`.github/workflows/ci.yml`) runs on GitHub-hosted runners: build + `cargo test` on Linux and macOS, `cargo fmt
--check` and clippy, `cargo audit`, a check that the committed `viewer/dist` matches `viewer/src`, `scripts/e2e.sh`
against a real Docker daemon, and the example tasks through
[node-q/hbench-action](https://github.com/node-q/hbench-action) with the latest release: every one must score 1 with
the oracle and 0 with `nop`. On Linux it also runs the OCI tests against two local `registry:3` containers
(`HBENCH_TEST_OCI_REGISTRY=localhost:5000 cargo test oci::tests`), the second taking basic auth with a password
generated for the run (`HBENCH_TEST_OCI_AUTH_REGISTRY=localhost:5001`,
`HBENCH_TEST_OCI_AUTH_CREDENTIALS=<user>:<password>`).
