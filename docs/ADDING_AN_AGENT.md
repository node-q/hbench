# Adding an agent

Each installed agent lives in `src/agents/installed/<name>.rs`, with any payload it uploads into the container under
`assets/agents/<name>/`. Use `src/agents/installed/claude_code.rs` as the worked example.

## Framework

- `src/agents/mod.rs`
  - `trait Agent { name, version, setup_timeout_sec, supports_resume, setup(cx), run(instruction, cx, ctx, resume), populate_context_post_run(logs_dir, ctx) }`
  - `AgentCtx`: `exec(cmd, env, timeout)` runs `bash -c` as the phase's default user (task `[agent].user`), with the
    agent `env` overlay applied last. `exec_as(cmd, Some("root"), ...)`. `cx.env` is the `DockerEnv`
    (`upload_file`, `upload_dir`, `download_file`, `exec_root`, ...). `cx.logs_dir` = host dir mirrored from `/logs/agent`.
  - `AgentArgs`: `model_name`, `kwargs` (JSON values from `--ak` / config `kwargs`), `extra_env` (`--ae`), `mcp_servers`,
    `skills`, `task`. Helpers: `kw(k)`, `kw_bool(k)`, `get_env(k)` (agent env > host env), `version_kw()`,
    `render_instruction()` (applies the `prompt_template_path` kwarg).
- `src/agents/installed/mod.rs`
  - `ensure_system_dependencies(cx, &["curl", "nodejs", ...])`: installs system packages from the shared package table.
  - `exec_agent(cx, cmd, env)` / `exec_root(...)`: prefix `set -o pipefail;`; a non-zero exit becomes a classified
    error (error patterns, last match wins).
  - `exec_error(cmd, result, Some(evidence))`: classify from filtered evidence (see claude-code's stream-json filter).
  - `filter_jsonl_events`, `parse_version`, `upload_text` (mode 600, owned by the agent user), and `connection(args, Spec)`.
- `src/agents/model_connection.rs`: provider/auth resolution (`Spec { default_provider, api_key_envs, base_url_envs,
  passthrough }`, `resolve`, `with_canonical_provider_envs`).

## What an agent implements

1. `pub fn create(args: AgentArgs) -> Result<AgentBox>`, registered in the factory. Validate options eagerly here.
2. `setup`: `mkdir -p /installed-agent` as root, install (pin versions via the `version` kwarg), then detect the version
   (`get_version_command` + `parse_version`).
3. `run`: render the instruction, build env (auth via model connection, model name mapping, agent-specific env vars,
   options → env) and CLI flags, with validated names, defaults and env fallbacks. Write logs to `/logs/agent/<file>`
   (`tee`). Upload secrets with `upload_text`.
4. Errors: if the agent's output needs filtering before classification, filter the evidence, then call `exec_error`.
5. `populate_context_post_run`: fill `AgentContext` (n_input_tokens incl. cache, n_cache_tokens, n_output_tokens,
   cost_usd) from the downloaded logs. ATIF trajectory conversion is optional.
6. Runner scripts uploaded into the container (`*_runner.py`, `.mjs`, wrappers) are embedded with `include_str!` from
   `assets/agents/<name>/`.
7. Register MCP servers and skills if the agent supports them.
8. `supports_resume`: true only if the agent can resume a session; implement the resume flag in `run`.

## Checks

- Unit tests in the agent file covering flag/env construction, option validation, and usage parsing (no docker needed).
- `cargo build`, `cargo test` and `cargo clippy --all-targets -- -D warnings` pass.
- `scripts/install-matrix.sh` installs the agent into a real container.
