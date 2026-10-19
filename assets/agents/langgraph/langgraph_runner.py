# Modified by the hbench authors.
"""Runs one LangGraph (Python) graph on a task instruction inside the task container.

Invoked by hbench's `langgraph` agent. Writes the raw final state (--result-path),
the answer text (--output-path) and an answer/token-usage summary (--summary-path).
"""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import importlib.util
import inspect
import json
import os
import sys
import uuid
from pathlib import Path
from typing import Any

USAGE = ("input_tokens", "output_tokens", "total_tokens")


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(description=__doc__)
    for flag in ("--project-dir", "--graph", "--graph-path", "--instruction-file", "--result-path", "--output-path"):
        ap.add_argument(flag, required=True)
    for flag in ("--summary-path", "--model", "--model-kwargs-json", "--configurable-json"):
        ap.add_argument(flag)
    return ap.parse_args()


def json_object(raw: str | None, what: str) -> dict[str, Any]:
    value = json.loads(raw) if raw else {}
    if not isinstance(value, dict):
        raise TypeError(f"{what} must be a JSON object")
    return value


def import_target(root: Path, spec: str) -> Any:
    """Import `./file.py:attr.path` from the project and return the attribute."""
    file_part, sep, attr = spec.partition(":")
    usage = f"Invalid graph path '{spec}'. Expected './path/to/file.py:attribute'"
    if not sep or not file_part or not attr or ":" in attr:
        raise ValueError(usage)
    if not all(name.isidentifier() for name in attr.split(".")):
        raise ValueError(f"Invalid graph attribute path '{attr}'. Expected dot-separated Python identifiers")

    root = root.resolve()
    source = (root / file_part).resolve()
    if not source.is_relative_to(root):
        raise ValueError(f"Graph module must resolve within the LangGraph project directory: {file_part}")
    if not source.is_file():
        raise FileNotFoundError(f"Graph module not found: {source}")

    sys.path.insert(0, str(root))
    name = f"hbench_graph_{uuid.uuid4().hex}"
    loader_spec = importlib.util.spec_from_file_location(name, source)
    if loader_spec is None or loader_spec.loader is None:
        raise ImportError(f"Could not import graph module: {source}")
    module = importlib.util.module_from_spec(loader_spec)
    sys.modules[name] = module
    loader_spec.loader.exec_module(module)

    obj: Any = module
    for name in attr.split("."):
        obj = getattr(obj, name)
    return obj


@contextlib.asynccontextmanager
async def build_graph(target: Any, config: dict[str, Any]):
    """A compiled graph is used as-is; anything else is a factory (sync/async, optionally
    taking the run config, optionally returning a context manager) that builds one."""
    if hasattr(target, "ainvoke") or hasattr(target, "invoke"):
        yield target
        return
    if not callable(target):
        raise TypeError("Graph target must be a compiled graph or a factory callable that returns one")
    try:
        wants_config = bool(inspect.signature(target).parameters)
    except (TypeError, ValueError):
        wants_config = True
    made = target(config) if wants_config else target()
    if hasattr(made, "__aenter__"):
        async with made as graph:
            yield graph
    elif hasattr(made, "__enter__"):
        with made as graph:
            yield graph
    else:
        yield await made if inspect.isawaitable(made) else made


def trace_parent():
    """Attach the run under a LangSmith parent trace when hbench was given one."""
    parent = os.environ.get("HBENCH_LANGSMITH_PARENT")
    if not parent:
        return contextlib.nullcontext()
    try:
        from langsmith.run_helpers import tracing_context  # only present if the project installs it
    except ImportError:
        print("HBENCH_LANGSMITH_PARENT is set but langsmith is not installed; the run will not be nested under it", file=sys.stderr)
        return contextlib.nullcontext()
    headers = {"langsmith-trace": parent}
    if os.environ.get("HBENCH_LANGSMITH_BAGGAGE"):
        headers["baggage"] = os.environ["HBENCH_LANGSMITH_BAGGAGE"]
    return tracing_context(parent=headers)


def plain(v: Any) -> Any:
    """JSON-safe copy of graph state; LangChain messages become small dicts."""
    if hasattr(v, "type") and hasattr(v, "content"):
        out = {"type": v.type, "content": plain(v.content)}
        for extra in ("tool_calls", "usage_metadata"):
            if getattr(v, extra, None):
                out[extra] = plain(getattr(v, extra))
        return out
    if isinstance(v, dict):
        return {str(k): plain(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [plain(x) for x in v]
    if v is None or isinstance(v, (str, int, float, bool)):
        return v
    return repr(v)


def answer_of(state: Any) -> str:
    """Content of the last message that has any, else the whole state as JSON."""
    messages = state.get("messages") if isinstance(state, dict) else None
    for m in reversed(messages if isinstance(messages, list) else []):
        if hasattr(m, "content"):
            c = m.content
        elif isinstance(m, dict) and m.get("content") is not None:
            c = m["content"]
        else:
            continue
        return c if isinstance(c, str) else repr(c)
    return json.dumps(plain(state), indent=2)


def token_usage(state: Any) -> dict[str, int]:
    totals = dict.fromkeys((*USAGE, "cache_read_tokens"), 0)
    messages = state.get("messages") if isinstance(state, dict) else None
    for m in messages or []:
        u = getattr(m, "usage_metadata", None)
        if not isinstance(u, dict):
            continue
        for k in USAGE:
            totals[k] += int(u.get(k) or 0)
        details = u.get("input_token_details")
        if isinstance(details, dict):
            totals["cache_read_tokens"] += int(details.get("cache_read") or 0)
    return totals


async def run(opts: argparse.Namespace) -> None:
    root = Path(opts.project_dir).resolve()
    prompt = Path(opts.instruction_file).read_text()
    target = import_target(root, opts.graph_path)

    configurable = json_object(opts.configurable_json, "configurable_json")
    kwargs = json_object(opts.model_kwargs_json, "model_kwargs_json")
    configurable.setdefault("thread_id", os.environ.get("HBENCH_SESSION_ID") or str(uuid.uuid4()))
    if opts.model:
        configurable.setdefault("model", opts.model)
    if kwargs:
        configurable.setdefault("model_kwargs", kwargs)
    config = {
        "configurable": configurable,
        "metadata": {"ls_runner": "hbench", "hbench_agent": "langgraph", "langgraph_graph": opts.graph},
    }

    async with build_graph(target, config) as graph:
        with trace_parent():
            inputs = {"messages": [{"role": "user", "content": prompt}]}
            if hasattr(graph, "ainvoke"):
                state = await graph.ainvoke(inputs, config=config)
            elif hasattr(graph, "invoke"):
                state = graph.invoke(inputs, config=config)
            else:
                raise TypeError("Selected graph must expose invoke() or ainvoke()")

    answer = answer_of(state)
    Path(opts.result_path).write_text(json.dumps(plain(state), indent=2))
    Path(opts.output_path).write_text(answer)
    if opts.summary_path:
        Path(opts.summary_path).write_text(json.dumps({"answer_written": answer, "usage": token_usage(state)}, indent=2))


if __name__ == "__main__":
    asyncio.run(run(parse_args()))
