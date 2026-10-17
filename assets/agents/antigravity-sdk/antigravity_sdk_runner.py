#!/usr/bin/env -S uv run --script
#
# /// script
# requires-python = ">=3.12"
# dependencies = [
#   "cryptography<47",
#   "fastapi",
#   "google-antigravity>=0.1.20",
#   "mcp>=1.27.2,<2",
# ]
# ///
"""hbench driver for the Google Antigravity SDK, run inside the task container.

Modes:
  --version          print the SDK version ("custom" for a local wheel)
  --mcp-sse-proxy    stdio <-> legacy-SSE MCP bridge (the SDK only speaks stdio/HTTP)
  default            solve --instruction, write an ATIF trajectory to --trajectory-path
"""

import argparse
import asyncio
import json
import logging
import os
import sys
from importlib.metadata import version as dist_version
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

PROXY_FLAG = "--mcp-sse-proxy"
PROXY_URL_VAR = "HBENCH_MCP_SSE_URL"
WHEEL_VAR = "ANTIGRAVITY_SDK_WHEEL_PATH"
SSE_IDLE_LIMIT = 86_400  # seconds; long tool calls keep the stream quiet

EFFORTS = ("minimal", "low", "medium", "high", "extra_high")

USAGE_NOTE = (
    "Token totals in final_metrics come from the SDK's session-wide counter "
    "(conversation.total_usage). That counter also sees model calls with no step "
    "of their own (progress updates, compaction, system turns), so it may be larger "
    "than the per-step metrics added up. Completion tokens count thinking tokens; "
    "final_metrics.extra keeps candidates and thoughts apart. When "
    "final_metrics.extra.accounting.complete is false the run ended early and the "
    "totals cover only the usage seen before that."
)


def sdk_version() -> str:
    if os.environ.get(WHEEL_VAR, "").strip():
        return "custom"
    return dist_version("google-antigravity")


def fail(msg: str, code: int = 1):
    print(f"Error: {msg}", file=sys.stderr)
    sys.exit(code)


# ---------------------------------------------------------------- MCP


def mcp_server(spec: dict[str, Any]) -> Any:
    from google.antigravity.types import McpStdioServer, McpStreamableHttpServer  # ty: ignore[unresolved-import]

    name = spec.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("MCP server requires a non-empty name")
    kind = spec.get("transport", "stdio")

    def need(field: str, label: str) -> str:
        value = spec.get(field)
        if not isinstance(value, str) or not value:
            raise ValueError(f"{label} MCP server {name!r} requires a {field if field == 'command' else 'URL'}")
        return value

    if kind == "stdio":
        return McpStdioServer(name=name, command=need("command", "stdio"), args=spec.get("args", []))
    if kind == "sse":
        # Re-launch this script as a stdio server that forwards to the SSE endpoint.
        url = need("url", "SSE")
        return McpStdioServer(name=name, command=sys.executable, args=[str(Path(__file__).resolve()), PROXY_FLAG], env={PROXY_URL_VAR: url})
    if kind in ("http", "streamable-http"):
        return McpStreamableHttpServer(name=name, url=need("url", "HTTP"))
    raise ValueError(f"Unsupported MCP transport: {kind!r}")


async def sse_bridge(url: str) -> None:
    import anyio
    from mcp.client.sse import sse_client
    from mcp.server.stdio import stdio_server

    parts = urlsplit(url)
    if parts.scheme not in ("http", "https") or not parts.netloc:
        raise ValueError("MCP SSE URL must be an absolute HTTP(S) URL")

    async with sse_client(url, sse_read_timeout=SSE_IDLE_LIMIT) as (up_rx, up_tx), stdio_server() as (local_rx, local_tx):
        async with anyio.create_task_group() as tg:

            async def pump(rx, tx) -> None:
                # Either direction closing ends the bridge.
                try:
                    async with tx:
                        async for msg in rx:
                            if isinstance(msg, Exception):
                                print(f"MCP SSE bridge: dropping unreadable message: {msg}", file=sys.stderr)
                            else:
                                await tx.send(msg)
                finally:
                    tg.cancel_scope.cancel()

            tg.start_soon(pump, local_rx, up_tx)
            tg.start_soon(pump, up_rx, local_tx)


# ---------------------------------------------------------------- usage → ATIF metrics


def _n(usage: Any, attr: str) -> int | None:
    return getattr(usage, attr, None)  # older SDKs lack some counters


def _generated(usage: Any) -> int | None:
    parts = [_n(usage, "candidates_token_count"), _n(usage, "thoughts_token_count")]
    if parts == [None, None]:
        return None
    return sum(p or 0 for p in parts)


def step_metrics(usage: Any) -> dict[str, Any]:
    return {
        "prompt_tokens": _n(usage, "prompt_token_count"),
        "completion_tokens": _generated(usage),
        "cached_tokens": _n(usage, "cached_content_token_count"),
        "extra": {"candidates_tokens": _n(usage, "candidates_token_count"), "thoughts_tokens": _n(usage, "thoughts_token_count")},
    }


def session_totals(usage: Any, finished: bool) -> dict[str, Any]:
    """final_metrics token fields; unknown stays null, never a fake zero."""
    accounting = {"source": "sdk_session_total_usage", "complete": finished}
    if usage is None:
        return {"total_prompt_tokens": None, "total_completion_tokens": None, "total_cached_tokens": None, "extra": {"accounting": accounting}}
    return {
        "total_prompt_tokens": _n(usage, "prompt_token_count"),
        "total_completion_tokens": _generated(usage),
        "total_cached_tokens": _n(usage, "cached_content_token_count"),
        "extra": {
            "candidates_tokens": _n(usage, "candidates_token_count"),
            "thoughts_tokens": _n(usage, "thoughts_token_count"),
            "sdk_total_tokens": _n(usage, "total_token_count"),
            "accounting": accounting,
        },
    }


def read_session_usage(conversation: Any) -> Any:
    # Called from cleanup; must not hide the error that got us here.
    try:
        return conversation.total_usage
    except Exception as e:
        print(f"Warning: could not read SDK session usage: {e}", file=sys.stderr)
        return None


def to_atif_step(step: Any, model: str) -> dict[str, Any] | None:
    from google.antigravity.types import StepSource, StepStatus  # ty: ignore[unresolved-import]

    if step.status != StepStatus.DONE or step.source == StepSource.USER:
        return None
    by_model = step.source == StepSource.MODEL
    out: dict[str, Any] = {"step_id": 0, "timestamp": None, "source": "agent" if by_model else "system", "message": step.content or ""}
    if not by_model:
        return out  # ATIF allows model fields on agent steps only
    out["model_name"] = model
    usage = getattr(step, "usage_metadata", None)
    if usage is not None:
        out["metrics"] = step_metrics(usage)
    if step.thinking:
        out["reasoning_content"] = step.thinking
    if step.tool_calls:
        out["tool_calls"] = [{"tool_call_id": c.id, "function_name": c.name, "arguments": c.args or {}} for c in step.tool_calls]
        results = [{"source_call_id": c.id, "content": c.output} for c in step.tool_calls if getattr(c, "output", None) is not None]
        if results:
            out["observation"] = {"results": results}
    return out


def save_trajectory(path: Path, steps: list[dict[str, Any]], usage: Any, finished: bool) -> None:
    try:
        agent_version = sdk_version()
    except Exception:
        agent_version = "0.1.20"
    for i, s in enumerate(steps, 1):
        s["step_id"] = i
    doc = {
        "schema_version": "ATIF-v1.7",
        "session_id": os.environ.get("SESSION_ID", "hbench-session"),
        "agent": {"name": "antigravity-sdk", "version": agent_version},
        "steps": steps,
        "final_metrics": {"total_steps": len(steps), "total_cost_usd": 0.0, **session_totals(usage, finished)},
        "notes": USAGE_NOTE,
    }
    # Write-then-rename so readers never see half a file.
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(doc, indent=2))
    os.replace(tmp, path)


# ---------------------------------------------------------------- agent run


def model_target(model: str, key: str, effort: str | None) -> tuple[Any, bool]:
    from google.antigravity.models import GeminiAPIEndpoint, GeminiModelOptions, ModelTarget, ThinkingLevel  # ty: ignore[unresolved-import]

    level = getattr(ThinkingLevel, effort.upper()) if effort and effort.lower() in EFFORTS else None
    options = GeminiModelOptions(thinking_level=level) if level is not None else GeminiModelOptions()
    return ModelTarget(name=model, endpoint=GeminiAPIEndpoint(api_key=key, options=options)), level is not None


async def solve(opts: argparse.Namespace) -> None:
    from google.antigravity import Agent, LocalAgentConfig  # ty: ignore[unresolved-import]
    from google.antigravity.hooks import policy  # ty: ignore[unresolved-import]

    logging.getLogger().setLevel(logging.ERROR)  # the SDK is noisy at WARNING

    full_model, key = os.environ.get("MODEL_NAME"), os.environ.get("GEMINI_API_KEY")
    if not key:
        fail("GEMINI_API_KEY environment variable not set")
    if not full_model:
        fail("MODEL_NAME environment variable not set")
    model = full_model.split("/")[-1]  # "google/gemini-x" -> "gemini-x"

    servers = [mcp_server(s) for s in json.loads(os.environ.get("MCP_SERVERS_JSON") or "[]")]
    target, has_level = model_target(model, key, os.environ.get("REASONING_EFFORT"))
    skills = None
    if os.environ.get("SKILLS_PATHS_JSON"):
        try:
            skills = json.loads(os.environ["SKILLS_PATHS_JSON"])
        except ValueError:
            pass

    logs = Path(opts.logs_dir).resolve()
    for d in (logs, logs / "session", logs / "app_data"):
        d.mkdir(parents=True, exist_ok=True)
    workspace = Path(opts.workspace.strip()).resolve() if (opts.workspace or "").strip() else Path.cwd().resolve()
    workspace.mkdir(parents=True, exist_ok=True)

    print(f"Starting Antigravity SDK Agent with instruction: {opts.instruction[:200]}...")
    print(f"Using model: {model}")
    if servers:
        print(f"MCP servers: {[s.name for s in servers]}")

    steps: list[dict[str, Any]] = [{"step_id": 1, "timestamp": None, "source": "user", "message": opts.instruction}]
    usage, finished = None, False
    trajectory = Path(opts.trajectory_path).resolve()
    home = Path.cwd()
    os.chdir(workspace)
    try:
        cfg = LocalAgentConfig(
            model=target,
            api_key=key,
            workspaces=[str(workspace)],
            save_dir=str(logs / "session"),
            app_data_dir=str(logs / "app_data"),
            mcp_servers=servers,
            policies=[policy.allow_all()],
            skills_paths=skills,
        )
        # An explicit thinking level must not be overridden by eval() defaults.
        cfg = cfg.eval(thinking_level=None) if has_level else cfg.eval()
        async with Agent(cfg) as agent:
            try:
                reply = await agent.chat(opts.instruction)
                (logs / "output.txt").write_text(await reply.text() or "")
                await reply.resolve()
                finished = True
            finally:
                for s in getattr(agent.conversation, "history", None) or []:
                    if (atif := to_atif_step(s, model)) is not None:
                        steps.append(atif)
                usage = read_session_usage(agent.conversation)
    finally:
        if home.exists():
            os.chdir(home)
        save_trajectory(trajectory, steps, usage, finished)
    print(f"Agent completed. Trajectory saved to {trajectory}")


def main() -> None:
    if "--version" in sys.argv:
        print(sdk_version())
        return
    if PROXY_FLAG in sys.argv:
        url = os.environ.get(PROXY_URL_VAR) or fail(f"{PROXY_URL_VAR} environment variable not set", 2)
        asyncio.run(sse_bridge(url))
        return
    cli = argparse.ArgumentParser(description="Run Google Antigravity SDK Agent")
    cli.add_argument("--instruction", required=True)
    cli.add_argument("--logs-dir", required=True)
    cli.add_argument("--trajectory-path", required=True)
    cli.add_argument("--workspace")
    asyncio.run(solve(cli.parse_args()))


if __name__ == "__main__":
    main()
