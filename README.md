# hbench

A fast, single-binary benchmark runner for coding-agent harnesses, written in Rust. It runs tasks in Docker, drives
40+ agents, grades them with task verifiers, and reports rewards, pass@k, tokens and cost. Tasks, datasets and job
configs use the [upstream](https://github.com/node-q/hbench) task format, so registry datasets such as
terminal-bench run as-is.

Install the latest release (Linux/macOS, x86_64/arm64; `HBENCH_VERSION=vX.Y.Z` pins, `HBENCH_INSTALL_DIR` overrides `~/.local/bin`):

```bash
curl -fsSL https://raw.githubusercontent.com/sapphirestd/harness-benchmark/master/scripts/install.sh | sh
```

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
uses a pinned LiteLLM price table. Claude Code works with an API key or a subscription token (`CLAUDE_CODE_OAUTH_TOKEN`, from `claude setup-token`).

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
  sidecar enforces them, and run-level `extra_allowed_hosts` are supported.
- Healthchecks, `workdir`, task env, agent/verifier users, resource limits, and GPUs.
- MCP servers and skills.
- `trajectory.json` / `load_trajectory` (native + ATIF), the simulated-user ACP bridge (`user_agent`), regrades, and
  diffs.

Some registry datasets build verifier-only data into the agent's image: LiveCodeBench's `/app/config.json` holds
the hidden tests, and Aider Polyglot and TB2's adaptive-rejection-sampler ship the reference solution encrypted with
the password in their public `solve.sh`. hbench removes these from the container before any agent but the oracle
starts (`src/integrity.rs`); the verifier still gets its own complete `tests/`.

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
`cargo test`, clippy, and a check that the committed `viewer/dist` matches `viewer/src`. Those runners have no docker
access, so run `scripts/e2e.sh` locally before a release.

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
