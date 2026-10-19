# Modified by the hbench authors.
"""Run a user-supplied Strands agent factory inside the task container.

hbench uploads a JSON config and the instruction, then calls
``strands_runner.py --config C --instruction I --result R --events E``.
Every message the agent adds is appended to E as JSONL; R gets one JSON
document describing the outcome (output, usage, observed model, tools, error).
"""

from __future__ import annotations

import argparse
import asyncio
import copy
import hashlib
import importlib
import inspect
import json
import os
import signal
import sys
import traceback
from collections.abc import Mapping
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from strands import Agent, SandboxTimeoutError
from strands.hooks import MessageAddedEvent
from strands.sandbox import ExecutionResult, PosixShellSandbox, StreamChunk
from strands.tools.mcp import MCPClient
from strands.vended_tools import make_file_editor, make_shell
from strands.vended_tools.file_editor.file_editor import DEFAULT_FILE_EDITOR_DESCRIPTION
from strands.vended_tools.shell.types import SANDBOX_SHELL_DESCRIPTION

# --------------------------------------------------------------------------- sandbox


def _kill_tree(proc: asyncio.subprocess.Process) -> None:
    if proc.returncode is None:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


class ContainerShell(PosixShellSandbox):
    """Runs the vended shell/editor tools directly in this container."""

    def __init__(self, workdir: str | None = None) -> None:
        self.workdir = workdir or os.getcwd()

    async def execute_streaming(self, command, *, timeout=None, cwd=None, env=None, **_):
        proc = await asyncio.create_subprocess_exec(
            "sh", "-c", command,
            cwd=cwd,
            env={**os.environ, **(env or {})},
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            start_new_session=True,  # own process group, so a timeout kills children too
        )
        chunks: asyncio.Queue[StreamChunk | None] = asyncio.Queue()
        captured: dict[str, list[str]] = {"stdout": [], "stderr": []}
        expired = False

        async def drain(reader: asyncio.StreamReader, kind: str) -> None:
            while block := await reader.read(65536):
                text = block.decode(errors="replace")
                captured[kind].append(text)
                chunks.put_nowait(StreamChunk(data=text, stream_type=kind))

        readers = [
            asyncio.create_task(drain(proc.stdout, "stdout")),
            asyncio.create_task(drain(proc.stderr, "stderr")),
        ]

        async def wait_exit() -> None:
            nonlocal expired
            try:
                await asyncio.wait_for(proc.wait(), timeout)
            except TimeoutError:
                expired = True
                _kill_tree(proc)
                await proc.wait()
            finally:
                await asyncio.gather(*readers, return_exceptions=True)
                chunks.put_nowait(None)

        watcher = asyncio.create_task(wait_exit())
        try:
            while (chunk := await chunks.get()) is not None:
                yield chunk
            if expired:
                raise SandboxTimeoutError(timeout)
            code = 1 if proc.returncode is None else proc.returncode
            yield ExecutionResult(
                exit_code=128 - code if code < 0 else code,
                stdout="".join(captured["stdout"]),
                stderr="".join(captured["stderr"]),
            )
        finally:
            _kill_tree(proc)
            if proc.returncode is None:
                await asyncio.shield(proc.wait())
            for task in (*readers, watcher):
                task.cancel()
            await asyncio.gather(*readers, watcher, return_exceptions=True)

    def get_tools(self) -> list[Any]:
        where = f"\n\nThe task working directory is {self.workdir}."
        return [
            make_file_editor(sandbox=self, name="sandbox_file_editor", description=DEFAULT_FILE_EDITOR_DESCRIPTION + where),
            make_shell(sandbox=self, name="sandbox_shell", description=SANDBOX_SHELL_DESCRIPTION + where),
        ]


# --------------------------------------------------------------------------- model

# provider -> constructor recipe. Paths are dotted keys into the constructor kwargs:
# "id" receives the model id, "url" the base URL, "env" maps a kwarg to env vars tried in order.
MODELS: dict[str, dict[str, Any]] = {
    "amazon-bedrock": {"cls": "bedrock.BedrockModel"},
    "anthropic": {"cls": "anthropic.AnthropicModel", "defaults": {"max_tokens": 8192}},
    "google": {"cls": "gemini.GeminiModel", "url": "client_args.http_options.base_url"},
    "llama": {"cls": "llamaapi.LlamaAPIModel"},
    "llamacpp": {
        "cls": "llamacpp.LlamaCppModel",
        "defaults": {"base_url": "http://localhost:8080"},
        "env": {"base_url": ["LLAMACPP_BASE_URL"]},
    },
    "litellm": {"cls": "litellm.LiteLLMModel"},
    "mistral": {"cls": "mistral.MistralModel", "url": "client_args.server_url"},
    "ollama": {
        "cls": "ollama.OllamaModel",
        "defaults": {"host": None},
        "env": {"host": ["OLLAMA_BASE_URL", "OLLAMA_API_BASE"]},
    },
    "openai": {"cls": "openai.OpenAIModel", "url": "client_args.base_url"},
    "openai-responses": {"cls": "openai_responses.OpenAIResponsesModel", "url": "client_args.base_url"},
    "sagemaker": {
        "cls": "sagemaker.SageMakerAIModel",
        "id": "endpoint_config.endpoint_name",
        "defaults": {"endpoint_config": {}, "payload_config": {"max_tokens": 8192, "stream": True}},
    },
    "writer": {"cls": "writer.WriterModel"},
}
PROVIDER_ALIASES = {
    "bedrock": "amazon-bedrock",
    "gemini": "google",
    "llama.cpp": "llamacpp",
    "llamaapi": "llama",
    "litellm_proxy": "openai",
    "openai_responses": "openai-responses",
}
# OpenAI-compatible endpoints that need their own API key variable.
OPENAI_LIKE = {"ovhcloud": "OVHCLOUD_API_KEY", "xai": "XAI_API_KEY"}


def _put(target: dict, dotted: str, value: Any) -> None:
    *parents, leaf = dotted.split(".")
    for key in parents:
        target = target.setdefault(key, {})
        if not isinstance(target, dict):
            raise ValueError(f"model_kwargs.{key} must be an object")
    target[leaf] = value


def _deep_merge(base: dict, extra: Mapping) -> dict:
    out = copy.deepcopy(base)
    for key, value in extra.items():
        if isinstance(out.get(key), dict) and isinstance(value, Mapping):
            out[key] = _deep_merge(out[key], value)
        else:
            out[key] = copy.deepcopy(value)
    return out


def _recipe(prefix: str | None, resolved: str | None) -> tuple[dict, str, bool]:
    """(recipe, provider label, pass the full model name as the id)."""
    keys = [PROVIDER_ALIASES.get(k, k) if k else k for k in (resolved, prefix)]
    for key in keys:
        if key in MODELS:
            return MODELS[key], key, False
    label = keys[0] or keys[1] or ""
    if label in OPENAI_LIKE:
        recipe = {**MODELS["openai"], "env": {"client_args.api_key": [OPENAI_LIKE[label]]}}
        return recipe, label, False
    return MODELS["litellm"], label or "litellm", True


def build_model(cfg: Mapping[str, Any]) -> Any | None:
    name = cfg.get("model_name")
    if name in (None, ""):
        return None
    if not isinstance(name, str):
        raise ValueError("Strands runner model_name must be a string")
    prefix, _, rest = name.partition("/") if "/" in name else (None, "", name)
    model_id = cfg.get("model_id")
    if not isinstance(model_id, str) or not model_id:
        model_id = rest

    resolved = cfg.get("model_provider")
    if resolved is not None and not isinstance(resolved, str):
        raise ValueError("Strands runner model_provider must be a string")
    if prefix is None and not resolved:
        raise ValueError(f"cannot determine a provider for model {name!r}; pass --model in provider/model-id form")
    overrides = cfg.get("model_kwargs") or {}
    if not isinstance(overrides, Mapping):
        raise ValueError("Strands runner model_kwargs must be a JSON object")

    recipe, label, full_name = _recipe(prefix, resolved)
    kwargs = copy.deepcopy(recipe.get("defaults", {}))
    for dotted, names in recipe.get("env", {}).items():
        value = next((os.environ[n] for n in names if os.environ.get(n)), None)
        if value is not None:
            _put(kwargs, dotted, value)

    base_url = cfg.get("model_base_url") or None
    if base_url is not None and not isinstance(base_url, str):
        raise ValueError("Strands runner model_base_url must be a string")
    if base_url and "url" in recipe:
        _put(kwargs, recipe["url"], base_url)

    kwargs = _deep_merge(kwargs, overrides)
    _put(kwargs, recipe.get("id", "model_id"), name if full_name else model_id)

    module, _, cls_name = recipe["cls"].rpartition(".")
    try:
        cls = getattr(importlib.import_module(f"strands.models.{module}"), cls_name)
    except (ImportError, AttributeError) as exc:
        raise ValueError(
            f"Strands provider {label!r} is unavailable. Ensure the selected strands-agents version includes that provider."
        ) from exc
    return cls(**kwargs)


# --------------------------------------------------------------------------- recording

SECRET_WORDS = {"credential", "password", "secret", "token"}
SECRET_SUBSTRINGS = ("access_key", "api_key", "private_key")


def _encode(value: Any) -> Any:
    if isinstance(value, bytes):
        return {"type": "bytes", "length": len(value), "sha256": hashlib.sha256(value).hexdigest()}
    if callable(getattr(value, "to_dict", None)):
        return value.to_dict()
    raise TypeError(f"{type(value).__name__} is not JSON serializable")


def _scrub(value: Any) -> Any:
    """Drop credential-looking keys and stringify anything that is not plain JSON."""
    if isinstance(value, Mapping):
        kept = {}
        for key, child in value.items():
            key = str(key)
            lower = key.lower()
            if SECRET_WORDS & set(lower.replace("-", "_").split("_")) or any(s in lower for s in SECRET_SUBSTRINGS):
                continue
            kept[key] = _scrub(child)
        return kept
    if isinstance(value, list | tuple):
        return [_scrub(v) for v in value]
    if value is None or isinstance(value, str | int | float | bool):
        return value
    return str(value)


def _describe_error(err: BaseException) -> dict[str, Any]:
    return {
        "type": type(err).__name__,
        "message": str(err),
        "traceback": "".join(traceback.format_exception(type(err), err, err.__traceback__)),
    }


def describe_model(agent: Agent) -> dict[str, Any]:
    model = agent.model
    try:
        config = model.get_config() if callable(getattr(model, "get_config", None)) else {}
    except Exception:
        config = {}
    seen = dict(config) if isinstance(config, Mapping) else {}
    model_id = next((seen[k] for k in ("model_id", "modelId", "model", "endpoint_name") if seen.get(k) is not None), None)
    if model_id is None:
        model_id = getattr(model, "model_id", None)

    client_args = getattr(model, "client_args", None)
    if isinstance(client_args, Mapping) and client_args:
        seen["client_args"] = dict(client_args)
    # First match wins, so the order below is the precedence.
    client = getattr(model, "client", None)
    client_meta = getattr(client, "meta", None)
    for key, value in (
        ("base_url", getattr(model, "base_url", None)),
        ("host", getattr(model, "host", None)),
        ("base_url", getattr(client, "base_url", None)),
        ("region_name", getattr(client_meta, "region_name", None)),
        ("base_url", getattr(client_meta, "endpoint_url", None)),
    ):
        if value is not None:
            seen.setdefault(key, str(value))

    kind = type(model)
    return {
        "provider": f"{kind.__module__}.{kind.__qualname__}",
        "model_id": None if model_id is None else str(model_id),
        "config": _scrub(seen),
    }


class Recorder:
    """Streams agent messages to the events file and writes the final result file."""

    def __init__(self, result: Path, events: Path) -> None:
        self.result_file, self.events_file = result, events
        self.count = 0
        self.report: dict[str, Any] = {
            "output": "",
            "usage": {},
            "checkpoint": None,
            "stop_reason": None,
            "observed_model": None,
            "tools": [],
            "error": None,
        }
        for p in (result, events):
            p.parent.mkdir(parents=True, exist_ok=True)
        result.unlink(missing_ok=True)
        events.write_text("")

    def _append(self, message: Mapping[str, Any], preexisting: bool = False) -> None:
        self.count += 1
        line = {"sequence": self.count, "kind": "message", "message": dict(message)}
        if preexisting:
            line["is_preexisting_context"] = True
        else:
            line["timestamp"] = datetime.now(UTC).isoformat()
        with self.events_file.open("a") as f:
            f.write(json.dumps(line, default=_encode, sort_keys=True) + "\n")

    def watch(self, agent: Agent) -> None:
        self.report["observed_model"] = describe_model(agent)
        tools = agent.tool_registry.get_all_tools_config()
        self.report["tools"] = [dict(c) for _, c in sorted(tools.items()) if isinstance(c, Mapping)]
        for message in agent.messages:
            self._append(message, preexisting=True)
        agent.add_hook(lambda event: self._append(event.message), MessageAddedEvent)

    def finish(self, outcome: Any) -> None:
        usage = getattr(getattr(outcome, "metrics", None), "accumulated_usage", None)
        checkpoint = getattr(outcome, "checkpoint", None)
        stop = getattr(outcome, "stop_reason", None)
        self.report.update(
            output=str(outcome).strip(),
            usage=dict(usage) if isinstance(usage, Mapping) else {},
            checkpoint=checkpoint.to_dict() if callable(getattr(checkpoint, "to_dict", None)) else checkpoint,
            stop_reason=None if stop is None else str(stop),
        )

    def failed(self, err: BaseException) -> None:
        self.report["error"] = _describe_error(err)

    def close_agent(self, agent: Any) -> None:
        if agent is None:
            return
        try:
            agent.cleanup()
        except Exception as err:
            if self.report["error"] is None:
                self.report["error"] = _describe_error(err)
            else:
                self.report["error"]["cleanup_error"] = _describe_error(err)

    def _dump(self, doc: dict[str, Any]) -> None:
        self.result_file.parent.mkdir(parents=True, exist_ok=True)
        self.result_file.write_text(json.dumps(doc, default=_encode, indent=2, sort_keys=True) + "\n")

    def save(self) -> dict[str, Any] | None:
        """Write the result; returns the error recorded (if any)."""
        try:
            self._dump(self.report)
            return self.report["error"]
        except Exception as err:
            # Something in usage/checkpoint/tools was not JSON: keep the rest.
            bad = _describe_error(err)
            prior = self.report["error"]
            error = {**prior, "result_serialization_error": bad} if prior is not None else {
                "type": "ResultSerializationError",
                "message": "Strands result contained non-JSON values",
                "traceback": bad["traceback"],
            }
            self._dump({**self.report, "usage": {}, "checkpoint": None, "tools": [], "error": error})
            return error


# --------------------------------------------------------------------------- entrypoint


def read_config(path: Path) -> dict[str, Any]:
    cfg = json.loads(path.read_text())
    if not isinstance(cfg, dict):
        raise ValueError("Strands runner config must be a JSON object")
    if not isinstance(cfg.get("factory"), str):
        raise ValueError("Strands runner config requires a string factory")
    for key in ("factory_kwargs", "mcp_servers"):
        cfg[key] = cfg.get(key) or {}
        if not isinstance(cfg[key], dict):
            raise ValueError(f"Strands runner {key} must be a JSON object")
    if cfg.get("workdir") is not None and not isinstance(cfg["workdir"], str):
        raise ValueError("Strands runner workdir must be a string")
    return cfg


def load_factory(ref: str) -> Any:
    module, sep, attr = ref.partition(":")
    if not (sep and module and attr):
        raise ValueError(f"factory must use the form 'module:symbol', got {ref!r}")
    return getattr(importlib.import_module(module), attr)


def make_agent(cfg: dict[str, Any], keep: list[Any]) -> Agent:
    """Build the agent; it is pushed onto `keep` as soon as it exists so it can be cleaned up."""
    ref = cfg["factory"]
    factory = load_factory(ref)
    kwargs = dict(cfg["factory_kwargs"])
    if "model" not in kwargs and (model := build_model(cfg)) is not None:
        kwargs["model"] = model
    sandbox = ContainerShell(cfg.get("workdir"))
    try:
        inspect.signature(factory).bind(sandbox, **kwargs)
    except (TypeError, ValueError) as exc:
        raise TypeError(f"Strands factory {ref!r} must accept (sandbox, model=None, **factory_kwargs)") from exc
    agent = factory(sandbox, **kwargs)
    keep.append(agent)
    if not isinstance(agent, Agent):
        raise TypeError(f"Strands factory {ref!r} returned {type(agent).__name__}; expected strands.Agent")
    if cfg["mcp_servers"]:
        agent.tool_registry.process_tools(MCPClient.load_servers(cfg["mcp_servers"]))
    return agent


async def invoke(opts: argparse.Namespace) -> int:
    rec = Recorder(Path(opts.result), Path(opts.events))
    built: list[Any] = []
    interrupt: BaseException | None = None
    try:
        agent = make_agent(read_config(Path(opts.config)), built)
        rec.watch(agent)
        rec.finish(await agent.invoke_async(Path(opts.instruction).read_text()))
    except BaseException as err:  # noqa: BLE001 - recorded, and re-raised below unless it is an Exception
        rec.failed(err)
        if not isinstance(err, Exception):
            interrupt = err
    finally:
        rec.close_agent(built[0] if built else None)
        error = rec.save()

    if interrupt is not None:
        raise interrupt
    if error is not None:
        print(error["traceback"], file=sys.stderr)
        return 1
    print(rec.report["output"])
    return 0


def main() -> int:
    cli = argparse.ArgumentParser()
    for flag in ("--config", "--instruction", "--result", "--events"):
        cli.add_argument(flag, required=True)
    return asyncio.run(invoke(cli.parse_args()))


if __name__ == "__main__":
    raise SystemExit(main())
