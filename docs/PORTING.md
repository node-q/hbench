# Porting a upstream agent to hbench

Reference source: `/tmp/hb-ref/hbench/src/hbench/agents/` (upstream, Python). The goal is behavior parity:
the same install commands, run command, CLI flags, env vars, auth resolution, and error classification.
Treat `src/agents/installed/claude_code.rs` as the worked example (port of `installed/claude_code.py`).

## Framework (already written, don't change it)

- `src/agents/mod.rs`
  - `trait Agent { name, version, setup_timeout_sec, supports_resume, setup(cx), run(instruction, cx, ctx, resume), populate_context_post_run(logs_dir, ctx) }`
  - `AgentCtx`: `exec(cmd, env, timeout)` runs `bash -c` as the phase's default user (task `[agent].user`), with the
    agent `env` overlay applied last (upstream's scoped_exec_env). `exec_as(cmd, Some("root"), ...)`. `cx.env` is the `DockerEnv`
    (`upload_file`, `upload_dir`, `download_file`, `exec_root`, ...). `cx.logs_dir` = host dir mirrored from `/logs/agent`.
  - `AgentArgs`: `model_name`, `kwargs` (JSON values from `--ak` / config `kwargs`), `extra_env` (`--ae`), `mcp_servers`,
    `skills`, `task`. Helpers: `kw(k)`, `kw_bool(k)`, `get_env(k)` (agent env > host env), `version_kw()`,
    `render_instruction()` (applies the `prompt_template_path` kwarg like upstream's `@with_prompt_template`).
- `src/agents/installed/mod.rs`
  - `ensure_system_dependencies(cx, &["curl", "nodejs", ...])`: same package table as upstream's `SYSTEM_PACKAGES`.
  - `exec_agent(cx, cmd, env)` / `exec_root(...)`: these prefix `set -o pipefail;`, and a non-zero exit becomes a classified
    error (upstream's `ERROR_PATTERNS`, last match wins). This is upstream's `exec_as_agent` / `exec_as_root`.
  - `exec_error(cmd, result, Some(evidence))`: classify from filtered evidence (see claude-code's stream-json filter).
  - `filter_jsonl_events`, `parse_version`, `upload_text` (mode 600, owned by the agent user), and `connection(args, Spec)`.
- `src/agents/model_connection.rs`: port of upstream's `model_connection.py` (`Spec { default_provider, api_key_envs,
  base_url_envs, passthrough }`, `resolve`, `with_canonical_provider_envs`). Use it wherever upstream uses
  `MODEL_CONNECTION` / `self.model_connection`.

## What a port must do

1. Replace the stub `pub fn create(args: AgentArgs) -> Result<AgentBox>` in your file. Keep that signature, because the
   factory already calls it. Validate options eagerly in `create`, as upstream does in `__init__`.
2. `setup`: upstream's `BaseInstalledAgent.setup` + `install()`. Run `mkdir -p /installed-agent` as root, then the install
   (same commands and URLs as upstream, version pinning via the `version` kwarg), then detect the version
   (`get_version_command` + `parse_version`).
3. `run`: render the instruction, build env (auth via model connection, model name mapping, agent-specific env vars,
   options → env), and build CLI flags from upstream's `options_model` / `CLI_FLAGS` / `ENV_VARS`, with the same names,
   defaults, env fallbacks, and validation. Write logs to `/logs/agent/<file>` as upstream does (`tee`). Upload configs the
   way upstream does (`upload_text` for secrets).
4. Errors: if upstream overrides `_classify_exec_error`, port the override (filter evidence, then `exec_error`).
5. `populate_context_post_run`: fill `AgentContext` (n_input_tokens incl. cache, n_cache_tokens, n_output_tokens,
   cost_usd) from the downloaded logs, as upstream does. When upstream derives these from an ATIF trajectory, compute the
   same totals directly from the native logs. Full ATIF conversion is optional.
6. Runner scripts that upstream uploads into the container (`*_runner.py`, `.mjs`, wrappers) are payload. Embed them
   verbatim with `include_str!` from a copy under `assets/agents/<name>/`, and upload them like upstream does.
7. MCP servers / skills: if upstream registers them for this agent, do the same.
8. `supports_resume`: true only if upstream's capabilities say `resume=True`. Implement the resume flag in `run`.

## Rules

- Edit only your agent files (`src/agents/installed/<name>.rs`, optional `assets/agents/<name>/...`). Don't touch
  `mod.rs`, `Cargo.toml`, or other agents. If you truly need a framework change, describe it in your final report instead.
- Add unit tests in your file covering flag/env construction, option validation, and usage parsing (no docker needed).
- `cargo build` must have no errors and `cargo test` must pass. Don't run docker.
- Match surrounding style: concise and idiomatic, no unnecessary abstractions.
