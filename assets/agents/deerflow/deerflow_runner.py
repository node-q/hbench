"""hbench entrypoint for DeerFlow inside the task container.

Reads the instruction from stdin, streams DeerFlow events to stdout as NDJSON and
leaves a small JSON report (status, token usage, error) at $DEERFLOW_SUMMARY_PATH.
DeerFlow swallows provider failures into a fallback reply; those exit non-zero here
so the host can classify and retry the trial.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

REPORT_VERSION = 1
TOKEN_KEYS = ("input_tokens", "output_tokens", "total_tokens")
TASK_DONE = frozenset({"task_completed", "task_failed", "task_cancelled", "task_timed_out"})
TRUTHY = frozenset({"1", "true", "yes", "on"})


def env_bool(key, default):
    raw = os.environ.get(key)
    return default if raw is None else raw.strip().lower() in TRUTHY


def env_int(key, default):
    try:
        return int(os.environ[key])
    except (KeyError, ValueError):
        return default


def count(v):
    """A token count: a non-negative int that is not a bool."""
    return type(v) is int and v >= 0


def tokens(raw):
    """Usage dict with all three counts, or None when anything is malformed."""
    if not isinstance(raw, dict):
        return None
    i, o = raw.get("input_tokens", 0), raw.get("output_tokens", 0)
    if not (count(i) and count(o)):
        return None
    t = raw.get("total_tokens", i + o)
    return {"input_tokens": i, "output_tokens": o, "total_tokens": t} if count(t) else None


def plus(*usages):
    return {k: sum(u[k] for u in usages) for k in TOKEN_KEYS}


class Usage:
    """Token bookkeeping over the event stream.

    The final `end` event is authoritative for the lead agent; without it we sum
    per-message metadata (first report per message id wins). Subagent tasks report
    separately and are always added on top.
    """

    def __init__(self):
        self.end = None
        self.by_msg = {}
        self.unkeyed = []
        self.by_task = {}

    def see(self, kind, data):
        if not isinstance(data, dict):
            return
        if kind == "end":
            u = tokens(data.get("usage"))
            if u is not None:
                self.end = u
        elif kind == "messages-tuple":
            u = tokens(data.get("usage_metadata"))
            if u is not None:
                mid = data.get("id")
                if isinstance(mid, str) and mid:
                    self.by_msg.setdefault(mid, u)
                else:
                    self.unkeyed.append(u)
        elif kind == "custom" and data.get("type") in TASK_DONE:
            tid, u = data.get("task_id"), tokens(data.get("usage"))
            if isinstance(tid, str) and tid and u is not None:
                self.by_task.setdefault(tid, u)

    def total(self):
        lead = self.end if self.end is not None else plus(*self.by_msg.values(), *self.unkeyed)
        return plus(lead, *self.by_task.values())


def text_or(v, default, cap):
    return v.strip()[:cap] if isinstance(v, str) and v.strip() else default


def provider_failure(node):
    """Depth-first search for DeerFlow's error-fallback marker on a message."""
    stack = [node]
    while stack:
        cur = stack.pop()
        if isinstance(cur, dict):
            extra = cur.get("additional_kwargs")
            if isinstance(extra, dict) and extra.get("deerflow_error_fallback"):
                body = text_or(cur.get("content"), "LLM provider failed after retries", 2000)
                return {
                    "type": text_or(extra.get("error_type"), "LLMError", 200),
                    "reason": text_or(extra.get("error_reason"), "generic", 200),
                    "detail": text_or(extra.get("error_detail"), body, 2000),
                }
            stack.extend(reversed(list(cur.values())))
        elif isinstance(cur, (list, tuple)):
            stack.extend(reversed(cur))
    return None


def say(msg):
    print(f"deerflow_runner: {msg}", file=sys.stderr)


def save_report(status, usage, error):
    dest = Path(os.environ.get("DEERFLOW_SUMMARY_PATH", "/logs/agent/deerflow_summary.json"))
    dest.parent.mkdir(parents=True, exist_ok=True)
    body = {"schema_version": REPORT_VERSION, "status": status, "usage": usage, "error": error}
    tmp = dest.with_name(f".{dest.name}.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(body, separators=(",", ":")) + "\n")
    tmp.replace(dest)


def run():
    # Heavy imports stay inside run() so the module loads without DeerFlow installed.
    from deerflow.client import DeerFlowClient
    from langgraph.errors import GraphRecursionError

    prompt = sys.stdin.read().strip()
    if not prompt:
        err = {"type": "ValueError", "reason": "runner_error", "detail": "empty instruction on stdin"}
        save_report("runner_error", plus(), err)
        say(err["detail"])
        return 2

    client = DeerFlowClient(
        model_name=os.environ.get("DEERFLOW_MODEL_NAME", "hbench-model"),
        subagent_enabled=env_bool("DEERFLOW_SUBAGENT_ENABLED", False),
        thinking_enabled=env_bool("DEERFLOW_THINKING_ENABLED", True),
        plan_mode=env_bool("DEERFLOW_PLAN_MODE", False),
    )
    usage, failure = Usage(), None
    status, error, code = "completed", None, 0
    try:
        for ev in client.stream(prompt, recursion_limit=env_int("DEERFLOW_RECURSION_LIMIT", 1000)):
            kind = str(ev.type)
            sys.stdout.write(json.dumps({"type": kind, "data": ev.data}, default=str) + "\n")
            sys.stdout.flush()
            usage.see(kind, ev.data)
            failure = failure or provider_failure(ev.data)
    except GraphRecursionError as e:
        status = "recursion_limit"
        say(f"recursion limit reached, stopping: {e}")
    except Exception as e:  # noqa: BLE001 - any crash becomes a runner_error report
        status, code = "runner_error", 1
        error = {"type": type(e).__name__, "reason": "runner_error", "detail": str(e)[:2000] or type(e).__name__}
        say(error["detail"])

    if failure is not None and status != "runner_error":
        status, code, error = "llm_error", 1, failure
        if failure["reason"] == "quota":
            say(f"specified API usage limits were reached: {failure['detail']}")
        else:
            say(f"LLM provider failure ({failure['reason']}): {failure['detail']}")

    save_report(status, usage.total(), error)
    return code


if __name__ == "__main__":
    sys.exit(run())
