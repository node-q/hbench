#!/usr/bin/env python3
"""Drive one prompt through an ACP agent and leave a record for hbench.

The agent is started via the launcher script and spoken to over stdio. Every
callback is appended to acp-events.jsonl; acp-summary.json collects the
handshake, auth/model negotiation, the prompt result and the last usage seen.
"""
from __future__ import annotations

import argparse
import asyncio
import codecs
import contextlib
import json
import os
import signal
import uuid
from pathlib import Path
from typing import Any

from acp import PROTOCOL_VERSION, RequestError, spawn_agent_process, text_block
from acp.interfaces import Client
from acp.schema import (
    AgentMessageChunk,
    AgentPlanUpdate,
    AgentThoughtChunk,
    AllowedOutcome,
    AuthCapabilities,
    ClientCapabilities,
    CreateTerminalResponse,
    DeniedOutcome,
    EnvVarAuthMethod,
    EnvVariable,
    FileSystemCapabilities,
    KillTerminalResponse,
    ReadTextFileResponse,
    ReleaseTerminalResponse,
    RequestPermissionResponse,
    SessionInfoUpdate,
    TerminalExitStatus,
    TerminalOutputResponse,
    ToolCall,
    ToolCallProgress,
    ToolCallStart,
    UsageUpdate,
    WaitForTerminalExitResponse,
    WriteTextFileResponse,
)

EVENTS_FILE = "acp-events.jsonl"
SUMMARY_FILE = "acp-summary.json"


def plain(obj: Any) -> Any:
    """SDK models and paths -> JSON-ready values."""
    if hasattr(obj, "model_dump"):
        return obj.model_dump(mode="json", by_alias=True, exclude_none=True)
    if isinstance(obj, Path):
        return str(obj)
    if isinstance(obj, dict):
        return {k: plain(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [plain(v) for v in obj]
    return obj


def bad_params(method: str, message: str, **extra: Any) -> RequestError:
    return RequestError.invalid_params({"method": method, "message": message, **extra})


def absolute(path: str, method: str) -> Path:
    p = Path(path)
    if not p.is_absolute():
        raise bad_params(method, "ACP paths must be absolute", path=path)
    return p


def tail_bytes(text: str, limit: int) -> tuple[str, bool]:
    """Keep the last `limit` UTF-8 bytes of `text`, never splitting a character."""
    raw = text.encode("utf-8")
    if len(raw) <= limit:
        return text, False
    if limit == 0:
        return "", True
    # A cut mid-character leaves 1-3 continuation bytes at the front; drop them.
    return raw[-limit:].decode("utf-8", errors="ignore"), True


def exit_status(proc: asyncio.subprocess.Process) -> TerminalExitStatus | None:
    code = proc.returncode
    if code is None:
        return None
    if code >= 0:
        return TerminalExitStatus(exit_code=code, signal=None)
    try:
        sig = signal.Signals(-code).name
    except ValueError:
        sig = str(-code)
    return TerminalExitStatus(exit_code=None, signal=sig)


class Terminal:
    """A child process whose merged stdout/stderr is buffered, optionally capped."""

    def __init__(self, session_id: str, proc: asyncio.subprocess.Process, cap: int | None) -> None:
        self.session_id = session_id
        self.proc = proc
        self.cap = cap
        self.buf = ""
        self.truncated = False
        self.pump: asyncio.Task[None] | None = None

    def feed(self, text: str) -> None:
        if not text:
            return
        self.buf += text
        if self.cap is not None:
            self.buf, cut = tail_bytes(self.buf, self.cap)
            self.truncated |= cut

    async def settle(self) -> None:
        if self.pump is not None:
            await self.pump


class Recorder(Client):
    def __init__(self, logs_dir: Path, permission_mode: str) -> None:
        self.events = logs_dir / EVENTS_FILE
        self.permission_mode = permission_mode
        self.permissions_requested = 0
        self.last_usage: dict[str, Any] | None = None
        self.last_session_info: dict[str, Any] | None = None
        self.terminals: dict[str, Terminal] = {}

    def log(self, kind: str, payload: Any) -> None:
        line = json.dumps({"event_type": kind, "payload": plain(payload)}, ensure_ascii=False)
        with self.events.open("a", encoding="utf-8") as fh:
            fh.write(line + "\n")

    def terminal(self, session_id: str, terminal_id: str) -> Terminal:
        t = self.terminals.get(terminal_id)
        if t is None or t.session_id != session_id:
            raise RequestError.resource_not_found(terminal_id)
        return t

    # -- session -------------------------------------------------------------

    def on_connect(self, conn: Any) -> None:
        self.log("on_connect", {"connection": type(conn).__name__})

    async def request_permission(self, options: list[Any], session_id: str, tool_call: ToolCall, **kwargs: Any) -> RequestPermissionResponse:
        self.permissions_requested += 1
        self.log("request_permission", {"session_id": session_id, "tool_call": tool_call, "options": options})
        allow = None
        if self.permission_mode != "deny":
            allow = next((o for o in options if o.kind in ("allow_once", "allow_always")), None)
        if allow is None:
            return RequestPermissionResponse(outcome=DeniedOutcome(outcome="cancelled"))
        return RequestPermissionResponse(outcome=AllowedOutcome(option_id=allow.option_id, outcome="selected"))

    async def session_update(self, session_id: str, update: Any, **kwargs: Any) -> None:
        self.log("session_update", {"session_id": session_id, "update": update})
        if isinstance(update, UsageUpdate):
            self.last_usage = plain(update)
        elif isinstance(update, SessionInfoUpdate):
            self.last_session_info = plain(update)

        # Human-readable transcript on stdout (tee'd into the agent log).
        line = None
        if isinstance(update, (AgentMessageChunk, AgentThoughtChunk)):
            text = getattr(update.content, "text", None)
            if text:
                line = text if isinstance(update, AgentMessageChunk) else f"[thought] {text}"
        elif isinstance(update, ToolCallStart):
            line = f"[tool] {update.title or update.tool_call_id} ({update.status or 'pending'})"
        elif isinstance(update, ToolCallProgress):
            line = f"[tool] {update.tool_call_id} -> {update.status or 'in_progress'}"
        elif isinstance(update, AgentPlanUpdate):
            line = "[plan update]"
        if line is not None:
            print(line, flush=True)

    async def ext_method(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.log("ext_method", {"method": method, "params": params})
        raise RequestError.method_not_found(method)

    async def ext_notification(self, method: str, params: dict[str, Any]) -> None:
        self.log("ext_notification", {"method": method, "params": params})

    # -- filesystem ----------------------------------------------------------

    async def write_text_file(self, content: str, path: str, session_id: str, **kwargs: Any) -> WriteTextFileResponse | None:
        target = absolute(path, "fs/write_text_file")
        self.log("write_text_file", {"session_id": session_id, "path": str(target), "content_length": len(content)})
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
        return WriteTextFileResponse()

    async def read_text_file(self, path: str, session_id: str, limit: int | None = None, line: int | None = None, **kwargs: Any) -> ReadTextFileResponse:
        method = "fs/read_text_file"
        source = absolute(path, method)
        if line is not None and line < 1:
            raise bad_params(method, "line must be >= 1", line=line)
        if limit is not None and limit < 0:
            raise bad_params(method, "limit must be >= 0", limit=limit)
        self.log("read_text_file", {"session_id": session_id, "path": str(source), "line": line, "limit": limit})
        try:
            text = source.read_text(encoding="utf-8")
        except FileNotFoundError as exc:
            raise RequestError.resource_not_found(str(source)) from exc
        if line is None and limit is None:
            return ReadTextFileResponse(content=text)
        first = (line or 1) - 1
        rows = text.splitlines(keepends=True)[first:]
        return ReadTextFileResponse(content="".join(rows if limit is None else rows[:limit]))

    # -- terminals -----------------------------------------------------------

    async def create_terminal(
        self,
        command: str,
        session_id: str,
        args: list[str] | None = None,
        cwd: str | None = None,
        env: list[EnvVariable] | None = None,
        output_byte_limit: int | None = None,
        **kwargs: Any,
    ) -> CreateTerminalResponse:
        method = "terminal/create"
        if not command.strip():
            raise bad_params(method, "command cannot be empty")
        if output_byte_limit is not None and output_byte_limit < 0:
            raise bad_params(method, "outputByteLimit must be >= 0", outputByteLimit=output_byte_limit)
        workdir = Path.cwd() if cwd is None else absolute(cwd, method)
        if not workdir.exists():
            raise RequestError.resource_not_found(str(workdir))

        terminal_id = f"term_{uuid.uuid4().hex}"
        argv = args or []
        self.log(
            "create_terminal",
            {
                "session_id": session_id,
                "terminal_id": terminal_id,
                "command": command,
                "args": argv,
                "cwd": str(workdir),
                "env": env or [],
                "output_byte_limit": output_byte_limit,
            },
        )
        child_env = {**os.environ, **{v.name: v.value for v in env or []}}
        try:
            proc = await asyncio.create_subprocess_exec(
                command, *argv, cwd=str(workdir), env=child_env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
            )
        except FileNotFoundError as exc:
            raise RequestError.resource_not_found(command) from exc
        except OSError as exc:
            raise RequestError.internal_error({"method": method, "message": str(exc), "command": command}) from exc

        term = Terminal(session_id, proc, output_byte_limit)
        self.terminals[terminal_id] = term
        term.pump = asyncio.create_task(self.pump(terminal_id, term))
        return CreateTerminalResponse(terminal_id=terminal_id)

    async def pump(self, terminal_id: str, term: Terminal) -> None:
        if term.proc.stdout is None:
            return
        utf8 = codecs.getincrementaldecoder("utf-8")(errors="replace")
        try:
            while chunk := await term.proc.stdout.read(4096):
                term.feed(utf8.decode(chunk))
            term.feed(utf8.decode(b"", final=True))
        except Exception as exc:
            self.log("terminal_output_error", {"terminal_id": terminal_id, "error": {"type": type(exc).__name__, "message": str(exc)}})
            raise

    async def terminal_output(self, session_id: str, terminal_id: str, **kwargs: Any) -> TerminalOutputResponse:
        term = self.terminal(session_id, terminal_id)
        status = exit_status(term.proc)
        self.log(
            "terminal_output",
            {
                "session_id": session_id,
                "terminal_id": terminal_id,
                "truncated": term.truncated,
                "has_exit_status": status is not None,
                "output_length": len(term.buf),
            },
        )
        return TerminalOutputResponse(output=term.buf, truncated=term.truncated, exit_status=status)

    async def wait_for_terminal_exit(self, session_id: str, terminal_id: str, **kwargs: Any) -> WaitForTerminalExitResponse:
        term = self.terminal(session_id, terminal_id)
        await term.proc.wait()
        await term.settle()
        status = exit_status(term.proc)
        if status is None:
            raise RequestError.internal_error(
                {"method": "terminal/wait_for_exit", "message": "Terminal exited without a final status", "terminalId": terminal_id}
            )
        self.log("wait_for_terminal_exit", {"session_id": session_id, "terminal_id": terminal_id, "exit_code": status.exit_code, "signal": status.signal})
        return WaitForTerminalExitResponse(exit_code=status.exit_code, signal=status.signal)

    async def kill_terminal(self, session_id: str, terminal_id: str, **kwargs: Any) -> KillTerminalResponse | None:
        term = self.terminal(session_id, terminal_id)
        if term.proc.returncode is None:
            with contextlib.suppress(ProcessLookupError):
                term.proc.kill()
            await term.proc.wait()
        await term.settle()
        self.log("kill_terminal", {"session_id": session_id, "terminal_id": terminal_id, "exit_status": exit_status(term.proc)})
        return KillTerminalResponse()

    async def release_terminal(self, session_id: str, terminal_id: str, **kwargs: Any) -> ReleaseTerminalResponse | None:
        term = self.terminal(session_id, terminal_id)
        if term.proc.returncode is not None:
            await term.settle()
        # A still-running process keeps pumping into this detached object; drop what it has.
        term.buf, term.truncated, term.cap = "", False, 0
        self.log("release_terminal", {"session_id": session_id, "terminal_id": terminal_id, "exit_status": exit_status(term.proc)})
        del self.terminals[terminal_id]
        return ReleaseTerminalResponse()


# -- negotiation helpers -----------------------------------------------------


def add_once(seq: list[str], item: str | None) -> None:
    if item and item not in seq:
        seq.append(item)


def spellings(model: str) -> list[str]:
    """`provider/name` is also tried as bare `name`."""
    out: list[str] = []
    model = model.strip()
    add_once(out, model)
    if "/" in model:
        add_once(out, model.split("/", 1)[1])
    return out


def pick_auth_method(policy: str, offered: list[Any] | None, explicit: str | None) -> str | None:
    if policy == "disabled":
        return None
    if policy == "explicit":
        return explicit
    if policy != "auto":
        raise ValueError(f"Unsupported ACP auth policy: {policy}")
    if explicit:
        return explicit
    # Auto: the first env-var method whose required variables are all set.
    for m in offered or []:
        if isinstance(m, EnvVarAuthMethod) and all(os.environ.get(v.name) for v in m.vars if not v.optional):
            return m.id
    return None


def candidates_from_session_models(wanted: str, models: Any) -> list[str]:
    """Ids to try with session/set_model, best match first."""
    names = spellings(wanted)
    offered = [m.model_id for m in getattr(models, "available_models", None) or [] if getattr(m, "model_id", None)]
    if not offered:
        return names
    # Agents that encode a variant as `model/variant` (e.g. reasoning effort) keep the current one.
    current = getattr(models, "current_model_id", None) or ""
    variant = current.split("/", 1)[1] if "/" in current else None
    out: list[str] = []
    for name in names:
        if name in offered:
            add_once(out, name)
        if variant and f"{name}/{variant}" in offered:
            add_once(out, f"{name}/{variant}")
        prefixed = [o for o in offered if o.startswith(name + "/")]
        if len(prefixed) == 1:
            add_once(out, prefixed[0])
    for name in names:
        add_once(out, name)
    return out


def model_config_option(session: Any) -> Any | None:
    for opt in getattr(session, "config_options", None) or []:
        if str(getattr(opt, "category", "")).casefold() == "model" or str(getattr(opt, "id", "")).casefold() in ("model", "model_id", "modelid"):
            return opt
    return None


def candidates_from_config_option(wanted: str, option: Any) -> list[str]:
    values: list[str] = []
    for item in getattr(option, "options", None) or []:
        if getattr(item, "value", None):
            add_once(values, item.value)
        else:  # an option group
            for sub in getattr(item, "options", None) or []:
                add_once(values, getattr(sub, "value", None))
    names = spellings(wanted)
    if not values:
        return names
    bare = lambda s: s.split("/", 1)[-1]  # noqa: E731
    out: list[str] = []
    for name in names:
        if name in values:
            add_once(out, name)
        same = [v for v in values if bare(v) == bare(name)]
        if len(same) == 1:
            add_once(out, same[0])
    return out


async def choose_model(conn: Any, session: Any, wanted: str, summary: dict[str, Any]) -> None:
    # The SDK connection exposes both setters whatever the agent supports, so go by what
    # the session advertised: legacy `models`, else a model-category config option.
    legacy = getattr(conn, "set_session_model", None)
    set_option = getattr(conn, "set_config_option", None)
    option = model_config_option(session)
    if getattr(session, "models", None) is not None and callable(legacy):
        summary["set_model_mechanism"] = "legacy_session_models"
        ids = candidates_from_session_models(wanted, session.models)
        apply = lambda mid: legacy(session_id=session.session_id, model_id=mid)  # noqa: E731
    elif option is not None and callable(set_option):
        summary["set_model_mechanism"] = "session_config_option"
        ids = candidates_from_config_option(wanted, option)
        apply = lambda mid: set_option(session_id=session.session_id, config_id=option.id, value=mid)  # noqa: E731
    else:
        summary["set_model_skipped"] = {"reason": "agent_did_not_advertise_model_selection"}
        summary["set_model_candidates"] = []
        message = "ACP agent did not advertise a model-selection mechanism"
        summary["set_model_error"] = {"message": message}
        raise RuntimeError(message)
    summary["set_model_candidates"] = ids

    attempts: list[dict[str, Any]] = []
    for mid in ids:
        try:
            resp = await apply(mid)
        except RequestError as exc:
            attempts.append({"model_id": mid, "error": {"code": exc.code, "message": str(exc), "data": exc.data}})
            continue
        attempts.append({"model_id": mid, "response": plain(resp)})
        summary["resolved_session_model_id"] = mid
        summary["set_model_response"] = plain(resp)
        break
    if attempts:
        summary["set_model_attempts"] = attempts
    if not ids:
        summary["set_model_error"] = {"message": "No compatible ACP session model candidate found"}
    elif "resolved_session_model_id" not in summary:
        summary["set_model_error"] = attempts[-1]["error"]


async def drive(instruction: str, launcher: str, logs_dir: Path) -> int:
    env = os.environ
    cwd = Path.cwd()
    policy = env.get("HBENCH_ACP_AUTH_POLICY", "auto")
    wanted_model = env.get("HBENCH_ACP_REQUESTED_MODEL")
    client = Recorder(logs_dir, env.get("HBENCH_ACP_PERMISSION_MODE", "allow"))
    summary: dict[str, Any] = {
        "registry_entry_id": env.get("HBENCH_ACP_AGENT_ID"),
        "registry_entry_version": env.get("HBENCH_ACP_AGENT_VERSION"),
        "workspace": str(cwd),
        "auth_policy": policy,
        "requested_model": wanted_model,
        "instruction": instruction,
    }
    status = 0
    try:
        async with spawn_agent_process(client, launcher, env=dict(env), cwd=cwd, transport_kwargs={"stderr": None}) as (conn, _):
            hello = await conn.initialize(
                protocol_version=PROTOCOL_VERSION,
                client_capabilities=ClientCapabilities(
                    auth=AuthCapabilities(terminal=False),
                    fs=FileSystemCapabilities(read_text_file=True, write_text_file=True),
                    terminal=True,
                ),
            )
            summary["initialize"] = plain(hello)
            summary["agent_info"] = plain(hello.agent_info)
            summary["auth_methods"] = plain(hello.auth_methods)

            method = pick_auth_method(policy, hello.auth_methods, env.get("HBENCH_ACP_AUTHENTICATE_METHOD_ID"))
            summary["selected_authenticate_method_id"] = method
            if method:
                summary["authenticate_response"] = plain(await conn.authenticate(method_id=method))

            session = await conn.new_session(cwd=str(cwd), mcp_servers=json.loads(env.get("HBENCH_ACP_MCP_SERVERS_JSON", "[]")))
            summary["session"] = plain(session)
            if wanted_model:
                await choose_model(conn, session, wanted_model, summary)

            reply = await conn.prompt(session_id=session.session_id, prompt=[text_block(instruction)])
            summary["prompt_response"] = plain(reply)
    except Exception as exc:
        summary["error"] = {"type": type(exc).__name__, "message": str(exc)}
        status = 1
    finally:
        summary["latest_usage_update"] = client.last_usage
        summary["latest_session_info_update"] = client.last_session_info
        summary["permissions_requested"] = client.permissions_requested
        summary["events_file"] = EVENTS_FILE
        (logs_dir / SUMMARY_FILE).write_text(json.dumps(summary, indent=2, ensure_ascii=False))
    return status


def main() -> int:
    ap = argparse.ArgumentParser(description="Send one prompt to an ACP agent and record the session")
    ap.add_argument("--instruction", required=True)
    ap.add_argument("--logs-dir", required=True)
    ap.add_argument("--launcher", required=True)
    opts = ap.parse_args()
    logs = Path(opts.logs_dir)
    logs.mkdir(parents=True, exist_ok=True)
    return asyncio.run(drive(opts.instruction, opts.launcher, logs))


if __name__ == "__main__":
    raise SystemExit(main())
