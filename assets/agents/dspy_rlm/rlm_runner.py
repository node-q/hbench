"""hbench host-side runner for upstream's dspy-rlm agent (agents/dspy_rlm.py).

dspy.RLM runs here, in the host Python, exactly as in upstream. Its environment tools are
proxied to hbench over stdio: one JSON request per line on the original stdout, one JSON
response per line on stdin. Everything else that writes to stdout goes to stderr.
"""

import json
import os
import shlex
import sys
import threading

_proto = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr
_lock = threading.Lock()


def _send(msg):
    _proto.write(json.dumps(msg, default=str) + "\n")
    _proto.flush()


def _request(msg):
    with _lock:
        _send(msg)
        line = sys.stdin.readline()
    if not line:
        raise RuntimeError("hbench bridge closed")
    resp = json.loads(line)
    if resp.get("error"):
        raise RuntimeError(resp["error"])
    return resp


class ExecResult:
    def __init__(self, return_code, stdout, stderr):
        self.return_code = return_code
        self.stdout = stdout
        self.stderr = stderr


def _format_exec_result(result, empty_msg="(no output)"):
    """Format an ExecResult into a human-readable string for RLM tools."""
    parts = []
    if result.stdout:
        parts.append(result.stdout)
    if result.stderr:
        parts.append(f"[stderr] {result.stderr}")
    if result.return_code != 0:
        parts.append(f"[exit code {result.return_code}]")
    return "\n".join(parts) if parts else empty_msg


class EnvironmentToolBridge:
    """Tools exposed to dspy.RLM (same names, docstrings and commands as upstream's bridge)."""

    def __init__(self, cwd="/", timeout_sec=30):
        self._cwd = cwd
        self._timeout_sec = timeout_sec

    def _exec(self, command, cwd=None):
        r = _request({"op": "exec", "command": command, "cwd": cwd or self._cwd, "timeout_sec": self._timeout_sec})
        return ExecResult(r.get("return_code", 0), r.get("stdout") or None, r.get("stderr") or None)

    def exec_command(self, command: str, cwd: str | None = None) -> str:
        """Execute a shell command in the environment. Returns stdout+stderr."""
        return _format_exec_result(self._exec(command, cwd))

    def read_file(self, path: str) -> str:
        """Read a file from the environment. Returns file contents."""
        result = self._exec(f"cat {shlex.quote(path)}")
        if result.return_code != 0:
            return f"[error] {result.stderr or 'file not found'}"
        return result.stdout or ""

    def write_file(self, path: str, content: str) -> str:
        """Write content to a file in the environment."""
        escaped = content.replace("'", "'\\''")
        result = self._exec(
            f'mkdir -p "$(dirname {shlex.quote(path)})" && '
            f"printf '%s' '{escaped}' > {shlex.quote(path)}"
        )
        if result.return_code != 0:
            return f"[error] {result.stderr or 'write failed'}"
        return "ok"

    def list_directory(self, path: str = ".") -> str:
        """List files and directories. Returns ls -la output."""
        result = self._exec(f"ls -la {shlex.quote(path)}")
        if result.return_code != 0:
            return f"[error] {result.stderr or 'directory not found'}"
        return result.stdout or ""

    def find_files(self, pattern: str, path: str = ".") -> str:
        """Find files matching a glob pattern."""
        result = self._exec(
            f"find {shlex.quote(path)} -name {shlex.quote(pattern)} -type f 2>/dev/null | head -50"
        )
        if result.return_code != 0:
            return f"[error] {result.stderr or 'find failed'}"
        return result.stdout or "(no matches)"

    def search_content(self, pattern: str, path: str = ".", file_glob: str = "") -> str:
        """Search file contents with grep. Returns matching lines."""
        glob_flag = f"--include={shlex.quote(file_glob)}" if file_glob else ""
        result = self._exec(
            f"grep -rn {glob_flag} {shlex.quote(pattern)} {shlex.quote(path)} 2>/dev/null | head -100"
        )
        if result.return_code != 0:
            return "(no matches)"
        return result.stdout or "(no matches)"

    def apply_patch(self, patch: str) -> str:
        """Apply a unified diff patch. The patch should be in unified diff format."""
        escaped = patch.replace("'", "'\\''")
        return _format_exec_result(
            self._exec(f"printf '%s' '{escaped}' | patch -p1 --no-backup-if-mismatch"),
            empty_msg="patch applied successfully",
        )

    def get_tools(self):
        return [
            self.exec_command,
            self.read_file,
            self.write_file,
            self.list_directory,
            self.find_files,
            self.search_content,
            self.apply_patch,
        ]


def _save_logs(prediction, logs_dir):
    rlm_dir = os.path.join(logs_dir, "rlm")
    os.makedirs(rlm_dir, exist_ok=True)
    keys = list(prediction.keys())
    solution = str(prediction[keys[0]]) if keys else str(prediction)
    with open(os.path.join(rlm_dir, "solution.txt"), "w") as f:
        f.write(solution)
    trajectory = getattr(prediction, "trajectory", None)
    if trajectory:
        with open(os.path.join(rlm_dir, "trajectory.json"), "w") as f:
            f.write(json.dumps(trajectory, indent=2, default=str))
    final_reasoning = getattr(prediction, "final_reasoning", None)
    if final_reasoning:
        with open(os.path.join(rlm_dir, "final_reasoning.txt"), "w") as f:
            f.write(str(final_reasoning))


def _usage(prediction, lm):
    """upstream's _populate_context, as a dict."""
    out = {}
    if prediction is None:
        return out
    try:
        usage = prediction.get_lm_usage()
        if usage:
            out["n_input_tokens"] = sum(u.get("input_tokens", 0) for u in usage.values())
            out["n_output_tokens"] = sum(u.get("output_tokens", 0) for u in usage.values())
    except (AttributeError, TypeError):
        pass
    try:
        cost = sum(x.get("cost", 0) or 0 for x in lm.history if isinstance(x, dict))
        if cost > 0:
            out["cost_usd"] = cost
    except (AttributeError, TypeError):
        pass
    trajectory = getattr(prediction, "trajectory", None)
    if trajectory:
        out["rlm_trajectory_steps"] = len(trajectory)
    return out


def main():
    cfg = json.loads(sys.stdin.readline())
    if cfg.get("op") == "version":
        try:
            import dspy

            _send({"op": "done", "version": dspy.__version__})
        except Exception:
            _send({"op": "done"})
        return
    try:
        import dspy
    except ImportError:
        _send({"op": "done", "kind": "DspyImportError", "error": "dspy is required for the dspy-rlm agent. Install it with: pip install 'hbench[dspy]'"})
        return
    lm = None
    prediction = None
    try:
        bridge = EnvironmentToolBridge(cfg["working_dir"], cfg["tool_timeout_sec"])
        lm = dspy.LM(cfg["model_name"], max_tokens=16_000)
        sub_lm = dspy.LM(cfg["sub_model_name"], max_tokens=8_000) if cfg.get("sub_model_name") else None
        rlm = dspy.RLM(
            signature=cfg["signature"],
            max_iterations=cfg["max_iterations"],
            max_llm_calls=cfg["max_llm_calls"],
            max_output_chars=cfg["max_output_chars"],
            verbose=cfg["verbose"],
            tools=bridge.get_tools(),
            sub_lm=sub_lm,
        )
        dspy.configure(lm=lm, track_usage=True)
        prediction = rlm(**cfg["input_kwargs"])
        _save_logs(prediction, cfg["logs_dir"])
        _send({"op": "done", **_usage(prediction, lm)})
    except Exception as e:
        _send({"op": "done", "kind": type(e).__name__, "error": str(e), **_usage(prediction, lm)})


if __name__ == "__main__":
    main()
