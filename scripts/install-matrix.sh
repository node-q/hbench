#!/usr/bin/env bash
# Install every installed agent into a real container (install_only: setup runs, no LLM calls),
# so install commands/URLs/versions are verified without API keys.
#   AGENTS="codex goose" scripts/install-matrix.sh     (default: all that need no user project)
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
BIN=$PWD/target/release/hbench
OUT=${OUT:-$(mktemp -d)}; mkdir -p "$OUT"
# Agents that wrap a user-supplied project/workflow (langgraph, deerflow, strands, eve, nemo-agent),
# a registry target (acp) or a desktop (computer-1) need inputs beyond a model, so they're opt-in.
DEFAULT="claude-code codex gemini-cli opencode aider goose copilot-cli cursor-cli openhands openhands-sdk mini-swe-agent
swe-agent trae-agent qwen-coder kimi-cli kimi-code grok-build hermes junie rovodev-cli cline-cli cortex-code code-puppy
devin mimo mcode pi openclaw antigravity-cli antigravity-sdk fx fx-dev vibe muse-code"
AGENTS=${AGENTS:-$DEFAULT}
model_for() {
  case $1 in
    codex|copilot-cli) echo openai/gpt-5 ;;
    gemini-cli|antigravity-cli|antigravity-sdk) echo google/gemini-2.5-pro ;;
    grok-build) echo xai/grok-4 ;;
    kimi-cli|kimi-code) echo kimi/kimi-k2-turbo-preview ;;
    vibe) echo mistral/devstral-medium-latest ;;
    qwen-coder) echo openai/qwen3-coder-plus ;;
    *) echo anthropic/claude-sonnet-4-5 ;;
  esac
}
{
  echo "jobs_dir: $OUT"; echo "job_name: install-matrix"; echo "n_concurrent_trials: ${N:-6}"; echo "install_only: true"
  echo "tasks: [{path: $PWD/examples/tasks/hello-world}]"
  echo "agents:"
  for a in $AGENTS; do
    echo "  - name: $a"; echo "    model_name: $(model_for "$a")"
    echo "    env: {ANTHROPIC_API_KEY: sk-ant-dummy, OPENAI_API_KEY: sk-dummy, GEMINI_API_KEY: dummy, GOOGLE_API_KEY: dummy, XAI_API_KEY: dummy, KIMI_API_KEY: dummy, MISTRAL_API_KEY: dummy, DEVIN_API_KEY: dummy, JUNIE_API_KEY: dummy, CURSOR_API_KEY: dummy, GITHUB_TOKEN: dummy, ROVODEV_USER_EMAIL: a@b.c, ROVODEV_USER_API_TOKEN: dummy, SNOWFLAKE_ACCOUNT: dummy, SNOWFLAKE_USER: dummy, SNOWFLAKE_PASSWORD: dummy, AI_GATEWAY_API_KEY: dummy, META_API_KEY: dummy, MIMO_API_KEY: dummy}"
  done
} > "$OUT/install-matrix.yaml"
"$BIN" run -c "$OUT/install-matrix.yaml" | tail -n 60
python3 - "$OUT/install-matrix" <<'EOF'
import json, sys, pathlib
bad = []
for r in sorted(pathlib.Path(sys.argv[1]).glob("*/result.json")):
    d = json.loads(r.read_text())
    a, v = d["agent_info"]["name"], d["agent_info"]["version"]
    e = d.get("exception_info")
    print(f"{'FAIL' if e else 'ok  '} {a:18} version={v:12} {(e or {}).get('exception_type', '')} {((e or {}).get('exception_message') or '')[:160]!r}" if e else f"ok   {a:18} version={v}")
    if e: bad.append(a)
print(f"{len(bad)} failed: {bad}")
sys.exit(1 if bad else 0)
EOF
