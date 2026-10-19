#!/usr/bin/env python3
# Modified by the hbench authors.
"""hbench driver for the OpenHands SDK: solve one instruction, then dump an ATIF trajectory.

Configured through env (LLM_*, LOAD_SKILLS, SKILL_PATHS, MCP_SERVERS_JSON, MAX_ITERATIONS, LITELLM_EXTRA_BODY).
"""

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any

from openhands.sdk import LLM, Agent, AgentContext, Conversation, Tool, get_logger
from openhands.sdk.context import Skill
from openhands.sdk.event import ActionEvent, MessageEvent, ObservationEvent, TokenEvent
from openhands.tools.file_editor import FileEditorTool
from openhands.tools.task_tracker import TaskTrackerTool
from openhands.tools.terminal import TerminalTool

log = get_logger(__name__)


def collect_skills(roots: list[str]) -> list[Skill]:
    """Every `<root>/<name>/SKILL.md`, always-on; the first root wins on a name clash."""
    found: dict[str, Skill] = {}
    for root in map(lambda r: Path(r).expanduser(), roots):
        if not root.exists():
            continue
        for md in (d / "SKILL.md" for d in root.iterdir() if d.is_dir()):
            name = md.parent.name
            if md.exists() and name not in found:
                found[name] = Skill(name=name, content=md.read_text(), source=str(md), trigger=None)
                log.debug(f"Loaded skill: {name} from {md}")
    return list(found.values())


def mcp_config(raw: str) -> dict[str, dict[str, Any]]:
    # The SDK (>=1.35) wants a flat {name: server} mapping, no "mcpServers" wrapper.
    out: dict[str, dict[str, Any]] = {}
    for spec in json.loads(raw):
        kind = spec.get("transport", "stdio")
        entry: dict[str, Any] = {}
        if kind == "stdio":
            entry.update({k: spec[k] for k in ("command", "args") if spec.get(k)})
        else:
            if spec.get("url"):
                entry["url"] = spec["url"]
            entry["transport"] = kind  # sse / http / streamable-http use the SDK's own names
        out[spec.get("name", "mcp-server")] = entry
    return out


def text_of(content: Any) -> str:
    if isinstance(content, list):
        return "\n".join(getattr(c, "text", str(c)) for c in content if getattr(c, "text", None))
    return str(content) if content else ""


def action_args(ev: ActionEvent) -> dict[str, Any]:
    fn = getattr(ev.tool_call, "function", None) if ev.tool_call else None
    raw = getattr(fn, "arguments", None)
    args: dict[str, Any] = {}
    if isinstance(raw, dict):
        args = raw
    elif isinstance(raw, str):
        try:
            args = json.loads(raw)
        except json.JSONDecodeError:
            args = {"raw": raw}
    if not args and ev.action:
        # No usable tool-call payload: fall back to the parsed action's fields.
        try:
            fields = ev.action.model_dump() if hasattr(ev.action, "model_dump") else vars(ev.action)
            args = {k: v for k, v in fields.items() if k != "kind" and v is not None}
        except Exception:
            pass
    return args


class StepLog:
    """Folds SDK conversation events into ATIF steps."""

    def __init__(self, model: str):
        self.model = model
        self.steps: list[dict[str, Any]] = []
        self.last_agent: dict[str, Any] | None = None

    def _agent_step(self, ts: Any, message: str) -> dict[str, Any]:
        step = {"step_id": 0, "timestamp": ts, "source": "agent", "message": message, "model_name": self.model}
        self.steps.append(step)
        self.last_agent = step
        return step

    def add(self, ev: Any) -> None:
        if isinstance(ev, MessageEvent):
            body = text_of(getattr(ev.llm_message, "content", None)) if ev.llm_message else ""
            if ev.source == "user":
                self.steps.append({"step_id": 0, "timestamp": ev.timestamp, "source": "user", "message": body})
            elif ev.source == "agent":
                self._agent_step(ev.timestamp, body)
        elif isinstance(ev, ActionEvent):
            step = self._agent_step(ev.timestamp, "")
            step["tool_calls"] = [{"tool_call_id": ev.tool_call_id, "function_name": ev.tool_name, "arguments": action_args(ev)}]
        elif isinstance(ev, ObservationEvent):
            # Only an observation that directly follows an agent step is kept (one result per step).
            if self.steps and self.steps[-1]["source"] == "agent":
                obs, body = ev.observation, ""
                if obs:
                    raw = getattr(obs, "content", None)
                    body = text_of(raw) if isinstance(raw, list) or raw else str(obs)
                self.steps[-1]["observation"] = {"results": [{"source_call_id": ev.tool_call_id, "content": body}]}
        elif isinstance(ev, TokenEvent) and self.last_agent is not None:
            self.last_agent["metrics"] = {
                "prompt_token_ids": getattr(ev, "prompt_token_ids", []),
                "completion_token_ids": getattr(ev, "response_token_ids", []),
            }

    def finish(self, system_prompt: str | None) -> list[dict[str, Any]]:
        if system_prompt:
            first_ts = self.steps[0]["timestamp"] if self.steps else None
            self.steps.insert(0, {"step_id": 0, "timestamp": first_ts, "source": "system", "message": system_prompt})
        for i, s in enumerate(self.steps, 1):
            s["step_id"] = i
        return self.steps


def build_llm(model: str, key: str) -> LLM:
    opts: dict[str, Any] = {"model": model, "api_key": key, "base_url": os.environ.get("LLM_BASE_URL")}
    if extra := os.environ.get("LITELLM_EXTRA_BODY"):
        # e.g. {"return_token_ids": true} for SGLang / vLLM backends
        if body := json.loads(extra):
            opts["litellm_extra_body"] = body
    if effort := os.environ.get("LLM_REASONING_EFFORT"):
        opts["reasoning_effort"] = effort
    if temp := os.environ.get("LLM_TEMPERATURE"):
        opts["temperature"] = float(temp)
    return LLM(**opts)


def main() -> None:
    cli = argparse.ArgumentParser(description="Run OpenHands SDK agent")
    cli.add_argument("--instruction")
    cli.add_argument("--instruction-file")
    cli.add_argument("--logs-dir", required=True)
    cli.add_argument("--trajectory-path", required=True)
    opts = cli.parse_args()
    task = Path(opts.instruction_file).read_text() if opts.instruction_file else opts.instruction
    if task is None:
        cli.error("--instruction or --instruction-file is required")

    model = os.environ.get("LLM_MODEL", "anthropic/claude-sonnet-4-5-20250929")
    key = os.environ.get("LLM_API_KEY")
    if not key:
        print("Error: LLM_API_KEY environment variable not set", file=sys.stderr)
        sys.exit(1)
    Path(opts.logs_dir).mkdir(parents=True, exist_ok=True)

    llm = build_llm(model, key)
    skills: list[Skill] = []
    if os.environ.get("LOAD_SKILLS", "1") == "1" and os.environ.get("SKILL_PATHS"):
        skills = collect_skills(os.environ["SKILL_PATHS"].split(":"))
    mcp = mcp_config(os.environ["MCP_SERVERS_JSON"]) if os.environ.get("MCP_SERVERS_JSON") else None

    agent = Agent(
        llm=llm,
        tools=[Tool(name=TerminalTool.name), Tool(name=FileEditorTool.name), Tool(name=TaskTrackerTool.name)],
        agent_context=AgentContext(skills=skills),
        **({"mcp_config": mcp} if mcp else {}),
    )
    max_iter = os.environ.get("MAX_ITERATIONS")
    # Workspace = the container's WORKDIR.
    conv = Conversation(agent=agent, workspace=os.getcwd(), **({"max_iteration_per_run": int(max_iter)} if max_iter else {}))

    print(f"Starting agent with instruction: {task[:200]}...")
    print(f"Using model: {model}")
    if os.environ.get("LLM_TEMPERATURE"):
        print(f"Temperature: {os.environ['LLM_TEMPERATURE']}")
    if max_iter:
        print(f"Max iterations per run: {max_iter}")
    print(f"Loaded {len(skills)} skills")
    if mcp:
        print(f"MCP servers: {list(mcp)}")

    conv.send_message(task)
    conv.run()

    usage = llm.metrics.accumulated_token_usage
    cost = llm.metrics.accumulated_cost
    system_prompt = None
    tools: list[dict[str, Any]] = []
    try:
        system_prompt = agent.static_system_message
    except Exception as e:
        log.debug(f"Could not extract system prompt: {e}")
    try:
        tools = [t.to_openai_tool() for t in agent.tools_map.values()]
    except Exception as e:
        log.debug(f"Could not extract tool definitions: {e}")
    if system_prompt:
        print(f"Captured system prompt ({len(system_prompt)} chars)")
    print(f"Captured {len(tools)} tool definitions")

    steps = StepLog(model)
    for ev in conv.state.events:
        steps.add(ev)
    doc = {
        "schema_version": "ATIF-v1.5",
        "session_id": os.environ.get("SESSION_ID", "hbench-session"),
        "agent": {"name": "openhands-sdk", "tool_definitions": tools or None, "version": "unknown"},
        "steps": steps.finish(system_prompt),
        "final_metrics": {
            "total_prompt_tokens": usage.prompt_tokens if usage else 0,
            "total_completion_tokens": usage.completion_tokens if usage else 0,
            "total_cached_tokens": usage.cache_read_tokens if usage else 0,
            "total_cost_usd": cost,
        },
    }
    out = Path(opts.trajectory_path)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(doc, indent=2))
    print(f"Agent completed. Trajectory saved to {out}")
    print(f"Total cost: ${cost:.4f}")


if __name__ == "__main__":
    main()
