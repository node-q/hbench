# hbench

hbench is the benchmark engine behind [Harness Playground](https://nodeq.tech), by node-q.

A fast, single-binary benchmark runner for coding-agent harnesses, written in Rust. It runs tasks in Docker, drives
40+ agents, grades them with task verifiers, and reports rewards, pass@k, tokens and cost. A task is a directory with
a `task.toml`, an environment, tests and a solution (see [Task format](#task-format)).

- [Installation](#installation)
- [Usage](#usage)
- [Examples](#examples)
- [Agents](#agents) · [Task format](#task-format) · [Output](#output) · [Troubleshooting](#troubleshooting)
- [Development](#development)
- [License](#license)
- [Contributors and contact](#contributors-and-contact)

## Installation

Requirements: a Docker daemon (local, or remote through `DOCKER_CONTEXT`/`DOCKER_HOST`) and Linux or macOS,
x86_64 or arm64.

Install the latest release:

```bash
curl -fsSL https://nodeq.tech/install.sh | sh
```

Release binaries are attached to [node-q/hbench's releases](https://github.com/node-q/hbench/releases); the script
downloads from there and checks `SHA256SUMS`. `HBENCH_VERSION=vX.Y.Z` pins a release (nodeq.tech defaults to the
version Harness Playground's arenas use; [`scripts/install.sh`](scripts/install.sh) on its own defaults to the latest),
and `HBENCH_INSTALL_DIR` overrides `~/.local/bin`.

Or build from source (the toolchain is pinned in `rust-toolchain.toml`):

```bash
cargo build --release
B=target/release/hbench
```

## Usage

```bash
$B run -p examples/tasks/hello-world                          # oracle on one task
$B run -d terminal-bench@2.0 -a oracle -n 8                    # registry dataset
$B run -c job.yaml                                             # job config (YAML/JSON)
$B run -c job.yaml --diff jobs/<old-job>                       # reuse / regrade / rerun vs an earlier job
$B jobs resume -p jobs/<job>        $B jobs summary -p jobs/<job>
$B jobs regrade --source jobs/<job> -p <tasks>                 # re-verify recorded outputs
$B trials start -p <task> -a codex -m openai/gpt-5             # single trial
$B trials handoff jobs/<job>/<trial>                           # resume the session locally (claude-code, muse-code)
$B sweeps run -c job.yaml --max-sweeps 3                       # drop solved tasks each sweep
$B view jobs [--port 8080-8089] [--tasks|--jobs]                # web viewer for jobs or task folders (also `view -p jobs`)
$B traces export -p jobs/<job> -o traces.jsonl                 # trace rows (ATIF episodes)
$B datasets list | download <name@ver> | init | add | remove
$B init <org/name> [--steps N]   $B add|remove|sync ...             # task/dataset authoring (dataset.toml)
$B tasks init|check|digest|start-env|migrate|schema               # digest: task_checksum + lock digest
$B analyze jobs/<job> | <trial>    $B check <tasks>                 # agent-run trajectory analysis / task QA
$B exec -i "summarize the notes" -p 'notes/*.md' --print-config   # experimental: compile prompt+paths into tasks
$B agents list | schema <agent>    $B trials schema    $B cache clean
$B completions zsh > ~/.zfunc/_hbench                          # shell completions (bash, zsh, fish, ...)
```

hbench has no default dataset registry. To run a named dataset (`-d name@version`), point it at a `registry.json`
with `--registry-url`, or at a local one with `--registry-path` (in a job config, the dataset's `registry_url` or
`registry_path`). Local task directories (`-p`) need neither.

Docker: hbench runs every trial in Linux containers on whatever `docker` points at (`DOCKER_CONTEXT`/`DOCKER_HOST`).
Remote ssh daemons work because files move with `docker compose cp` rather than bind mounts.

`run`, `jobs resume`, `trials start` and `sweeps run` exit 130 when cancelled (SIGINT, SIGTERM or SIGHUP; a second
signal force-quits after removing the run's compose projects), 1 on an error or when every trial errored
(`--fail-on-error`: any trial), and 0 otherwise. Before starting they check agent kwargs against the agent's schema and,
for `claude-code`, `codex` and `gemini-cli`, that some credential is set; `--skip-preflight` skips the credential check.
For `sweeps run`, every trial means every trial of all its sweeps.

## Examples

Check the setup with the oracle agent, which runs each task's reference solution:

```bash
$B run -p examples/tasks/hello-world
```

Run Claude Code on a sample of a registry dataset, 3 attempts per task, only the tasks matching `regex*`:

```bash
ANTHROPIC_API_KEY=... $B run -d terminal-bench-sample@2.0 --registry-url <registry.json URL> \
  -a claude-code -m anthropic/claude-opus-5-5 -k 3 -i 'regex*'
```

Run DeepSeek's own harness on a full dataset:

```bash
DEEPSEEK_API_KEY=... $B run -d terminal-bench@2.0 --registry-url <registry.json URL> \
  -a deepseek-harness -m deepseek/deepseek-v4-flash
```

Browse the results in the web viewer:

```bash
$B view jobs
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
installed. `scripts/dev/check-pins.py` lists the pins with a newer release. Claude Code works with an API key or a subscription token (`CLAUDE_CODE_OAUTH_TOKEN`, from `claude setup-token`).

`deepseek-harness` runs DeepSeek's own harness ([`dsh`](https://github.com/deepseek-ai/deepseek-harness),
npm `@deepseek-ai/dsh`) in its headless profile. It needs `DEEPSEEK_API_KEY` (optional `DEEPSEEK_BASE_URL`, an
Anthropic-Messages root). Pass the model as `-m deepseek/<id>` (default: dsh's `deepseek-flash`) and optionally
`--ak reasoning_effort=off|low|high|max`. It writes `trajectory.json` (ATIF), token usage and LiteLLM cost from dsh's
session logs, and supports `resume_trajectory` for multi-step tasks.

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
Other hbench variables: `DOCKER_CONTEXT`/`DOCKER_HOST` (which daemon), `HBENCH_EGRESS_METHOD=proxy` (see below).

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

## Output

```
jobs/<job>/{config.json, lock.json, result.json, job.log, trials.json}
jobs/<job>/<task>__<id>/{config.json, lock.json, result.json, trial.log, exception.txt,
                         agent/, verifier/, artifacts/{manifest.json, logs/artifacts/...}, steps/<name>/...}
```

`trials.json` is written only for jobs not planned from their config (`jobs regrade`, `run --diff`), so they can be
resumed. The job's `result.json` has per-eval metrics (mean/sum/min/max/uv-script), pass@k, reward/exception stats, tokens,
and cost. Failed trials are retried by error type (`--max-retries`, include/exclude).

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

## Troubleshooting

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
  and scrubbed from the job), and checks for leaked containers.
- `scripts/install-matrix.sh`: installs every installed agent into a real container (`install_only`).

CI (`.github/workflows/ci.yml`) runs on GitHub-hosted runners: build + `cargo test` on Linux and macOS, `cargo fmt
--check` and clippy, `cargo audit`, a check that the committed `viewer/dist` matches `viewer/src`, and `scripts/e2e.sh`
against a real Docker daemon.

### Releases

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

## License

Apache-2.0. See [LICENSE](LICENSE).

## Contributors and contact

hbench is built and maintained by [node-q](https://nodeq.tech). See the
[contributors graph](https://github.com/node-q/hbench/graphs/contributors) for everyone who has contributed.

Report bugs and request features in [GitHub issues](https://github.com/node-q/hbench/issues). To add an agent, see
[docs/ADDING_AN_AGENT.md](docs/ADDING_AN_AGENT.md).
