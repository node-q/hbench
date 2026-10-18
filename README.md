# hbench

A fast, single-binary benchmark runner for coding-agent harnesses, written in Rust. It runs tasks in Docker, drives
40+ agents, grades them with task verifiers, and reports rewards, pass@k, tokens and cost. Tasks, datasets and job
configs use the [upstream](https://github.com/node-q/hbench) task format, so registry datasets such as
terminal-bench run as-is.

Install the latest release (Linux/macOS, x86_64/arm64):

```bash
curl -fsSL https://nodeq.tech/install.sh | sh
```

The binaries are public in [node-q/hbench-releases](https://github.com/node-q/hbench-releases); the script downloads
from there and checks `SHA256SUMS`. `HBENCH_VERSION=vX.Y.Z` pins a release (nodeq.tech defaults to the version the
arenas pin; [`scripts/install.sh`](scripts/install.sh) on its own defaults to the latest), and `HBENCH_INSTALL_DIR`
overrides `~/.local/bin`. If the public download fails and `gh` is signed in, it falls back to this repo's releases.

Or build from source:

```bash
cargo build --release
B=target/release/hbench

$B run -p examples/tasks/hello-world                          # oracle on one task
$B run -d terminal-bench@2.0 -a oracle -n 8                    # registry dataset
$B run -d terminal-bench-sample@2.0 -a claude-code -m anthropic/claude-opus-5-5 -k 3 -i 'regex*'
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
$B exec "summarize ./notes/*.md" --print-config                  # experimental: compile prompt+paths into tasks
$B agents list | schema <agent>    $B trials schema    $B cache clean
```

Docker: hbench uses whatever `docker` points at (`DOCKER_CONTEXT`/`DOCKER_HOST`). Remote ssh daemons work because
files move with `docker compose cp` rather than bind mounts.

## Agents

47 agents: `oracle`, `nop`, `terminus-2` (also `terminus`, `terminus-1`), `claude-code`, `codex`,
`gemini-cli`, `opencode`, `aider`, `goose`, `copilot-cli`, `cursor-cli`, `openhands`, `openhands-sdk`, `mini-swe-agent`,
`swe-agent`, `trae-agent`, `qwen-coder`, `kimi-cli`, `kimi-code`, `grok-build`, `hermes`, `junie`, `rovodev-cli`,
`cline-cli`, `cortex-code`, `code-puppy`, `devin`, `langgraph`, `deerflow`, `strands`, `nemo-agent`, `mimo`, `mcode`,
`pi`, `openclaw`, `antigravity-cli`, `antigravity-sdk`, `eve`, `fx`, `fx-dev`, `vibe`, `muse-code`, `acp` (+ `acp:<id>`),
`computer-1`, `dspy-rlm`.

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

```bash
DEEPSEEK_API_KEY=... $B run -d terminal-bench@2.0 -a deepseek-harness -m deepseek/deepseek-v4-flash
```

## Task format

```
task/
├── task.toml        # [task] [agent] [verifier] [environment] [[steps]] artifacts=[...] ...
├── instruction.md   # or steps/<name>/instruction.md for multi-step tasks
├── environment/     # Dockerfile and/or docker-compose.yaml (`main` = agent container)
├── tests/test.sh    # writes /logs/verifier/reward.txt or reward.json
└── solution/solve.sh
```

Supported task features:

- Multi-step tasks: per-step workdir/setup.sh, healthcheck, `min_reward` gates, mean/final reward strategy, and resume.
- Shared and separate verifier environments, including bundled verifier images.
- Artifacts: sidecar sources, destinations, excludes, `manifest.json`, and `[[verifier.collect]]` hooks.
- Network policies: `public`, `no-network`, and `allowlist`, phase-scoped per agent/verifier/step. An egress
  sidecar enforces them, and run-level `extra_allowed_hosts` are supported. As in upstream, those hosts only
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

Unlike upstream, hbench rejects keys it does not know in a job config or `task.toml`, so a misspelt
`enforce_allowlist` stops the run instead of leaving the agent's network open. The legacy spellings upstream still
accepts are rewritten first: `orchestrator.{n_concurrent_trials,quiet,retry}` move to the top level, a dataset's
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
or a service manager), keep running for the verifier, as in upstream. If they cannot be stopped, the trial ends
ungraded with the run's own error (`AgentTimeoutError` after a timeout). In a shared verifier environment such a process
could also change files while the tests run; it shows up in the trajectory, so review trajectories of top runs.

## Output

```
jobs/<job>/{config.json, lock.json, result.json}
jobs/<job>/<task>__<id>/{config.json, lock.json, result.json, trial.log, exception.txt,
                         agent/, verifier/, artifacts/{manifest.json, logs/artifacts/...}, steps/<name>/...}
```

`result.json` has JobStats: per-eval metrics (mean/sum/min/max/uv-script), pass@k, reward/exception stats, tokens, and
cost. Failed trials are retried by error type (`--max-retries`, include/exclude).

Agent setup is hardened so environment problems aren't charged to the agent:

- A failed agent install is retried (3 tries within the setup timeout).
- If it still fails, it's recorded as `AgentSetupError` ("failed before the agent started") rather than as an agent
  failure.
- apt installs fall back to the image's sources minus the `*-security` suites. This matters for EOL releases like
  Debian bullseye, whose security index lists packages that are gone from the mirror.

## Viewer

`hbench view` serves its own web UI (`viewer/`, React + Vite) for browsing jobs and tasks. The built bundle
(`viewer/dist`) is committed and embedded at compile time, so `cargo build` doesn't need Node. After changing the UI:

```bash
cd viewer && npm install && npm run build      # type-check + rebuild viewer/dist
npm run dev                                    # hot reload; proxies /api to `hbench view <folder> --port 8091`
```

## Not supported

These require external services:

- Cloud sandboxes (Daytona, Modal, E2B, ...).
- Hosted package registries (publish/upload, `org/name@ref` packages).
- Windows containers.

Benchmark adapters aren't needed: they produce task dirs, which hbench runs directly, and converted datasets
are in the registry.

## Verification

- `cargo test`: unit tests for every module and agent.
- `scripts/e2e.sh`: a fixture suite against a real docker daemon. It covers timeouts, missing rewards, reward.json,
  users/env, no-network, compose sidecars, and prebuilt images, and checks for leaked containers.
- `scripts/conformance.sh`: runs the task-format reference `examples/tasks` (`REF=<checkout>`) and checks the expected
  outcomes, including the network-policy matrix, verifier-mode matrix, multi-step, sidecar artifacts, and extra allowed hosts.
- `scripts/parity.py <reference_job> <hbench_job>`: diffs a reference job with an hbench job on the same tasks. It
  compares rewards, exceptions, the `result.json` key structure, the trial file layout, and artifact manifests.
- `scripts/install-matrix.sh`: installs every installed agent into a real container (`install_only`).

CI (`.github/workflows/ci.yml`) runs on the org's self-hosted Linux runners (`[self-hosted, linux, x64, hpg]`): build +
`cargo test`, clippy, a check that the committed `viewer/dist` matches `viewer/src`, and `scripts/e2e.sh`.

## Releases

Bump `version` in `Cargo.toml`, commit, then push a matching tag:

```bash
git tag v0.2.0 && git push origin v0.2.0
```

`.github/workflows/release.yml` checks the tag against `Cargo.toml`, runs the tests, builds static x86_64/aarch64 Linux
binaries, and publishes a GitHub release with `.tar.gz` archives, `SHA256SUMS`, and generated notes. Tags with a suffix
(`v0.2.0-rc.1`) are published as prereleases. The runners are Linux-only, so add the macOS binaries from a Mac with the
tag checked out:

```bash
scripts/release-macos.sh v0.2.0
```

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
