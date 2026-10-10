# hbench

A Rust agent-harness benchmark runner, compatible with [upstream](https://github.com/node-q/hbench) tasks, datasets, and its result layout.

```bash
cargo build --release
B=target/release/hbench

# Oracle (reference solutions) on a local task or a dir of tasks
$B run -p examples/tasks/hello-world
# Registry dataset (upstream's registry.json), 4 concurrent trials
$B run -d terminal-bench@2.0 -a oracle -n 4
# Claude Code, 3 attempts per task (pass@k), selected tasks
ANTHROPIC_API_KEY=... $B run -d terminal-bench-sample@2.0 -a claude-code -m anthropic/claude-opus-5-5 -k 3 -i 'regex*'

$B jobs resume -p jobs/<job>     # rerun unfinished/cancelled trials
$B jobs summary -p jobs/<job>
$B datasets list
```

Main flags (named as in `hbench run`): `-a/--agent {oracle,nop,claude-code}`, `-m/--model`, `-k/--n-attempts`,
`-n/--n-concurrent`, `--ae KEY=VALUE`, `-i/-x` include/exclude task globs, `-l/--n-tasks`, `--timeout-multiplier`,
`--max-retries`, `--force-build`, `--disable-verification`, `--jobs-dir`, `--job-name`, `--registry-path`.

## Task format (same as upstream)

```
task/
├── task.toml        # [agent] timeout_sec/user, [verifier] timeout_sec/env/user,
│                    # [environment] docker_image/build_timeout_sec/cpus/memory_mb/gpus/workdir/env/network_mode
├── instruction.md
├── environment/     # Dockerfile and/or docker-compose.yaml (extra services; `main` is the agent container)
├── tests/test.sh    # writes /logs/verifier/reward.txt (one number) or reward.json ({"name": number})
└── solution/solve.sh  # used by the oracle agent
```

## Trial lifecycle

1. `docker compose build` (once per image per run) and `up --wait`. The `main` service runs `sleep infinity`.
2. Agent setup, then the agent run (with the agent timeout). Timeouts and non-zero exits are recorded, and the verifier still runs.
3. `tests/` is copied to `/tests`, then `/tests/test.sh` runs and `/logs/verifier` is copied back to parse the reward.
4. `/logs/agent` and `/logs/artifacts` are copied back, then `docker compose down --volumes`.

Files move with `docker compose cp`, not bind mounts, so remote daemons work (`DOCKER_CONTEXT=...`).

## Output

```
jobs/<job>/config.json, result.json        # JobStats: mean metric, pass@k, reward/exception stats, tokens, cost
jobs/<job>/<task>__<id>/config.json, result.json, trial.log, exception.txt,
                        agent/, verifier/ (reward.*, test-stdout.txt), artifacts/
```

Error types follow upstream's names (AgentTimeoutError, VerifierTimeoutError, RewardFileNotFoundError, ...). Retries
(`--max-retries`) skip the same non-retryable set that upstream skips.

## Not implemented (yet)

These upstream features are missing: multi-step tasks, cloud sandboxes (Daytona/Modal/...), network allowlists, artifact
configs, ATIF trajectories, and agents other than oracle/nop/claude-code. Add them when a benchmark needs them.

## Tests

`cargo test` runs the unit tests. `scripts/e2e.sh` runs the fixture suite against a real docker daemon.
