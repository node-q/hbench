# Modified by the hbench authors.
"""Host-side dspy.RLM driver for hbench's dspy-rlm agent.

hbench starts this with `python3 -c` and speaks newline-delimited JSON with it: the
first stdin line is the run config; afterwards every tool call is sent on the real
stdout as {"op": "exec", ...} and answered on stdin. The run ends with one
{"op": "done", ...} message carrying usage and, on failure, the exception.
"""

import json
import os
import shlex
import sys
import threading

# Keep fd 1 for the protocol only; anything dspy or user code prints goes to stderr.
wire = os.fdopen(os.dup(1), "w")
os.dup2(2, 1)
sys.stdout = sys.stderr
wire_lock = threading.Lock()


def emit(msg):
    wire.write(json.dumps(msg, default=str) + "\n")
    wire.flush()


def remote(command, cwd, timeout):
    """Run `command` in the task environment via hbench; returns (code, stdout, stderr)."""
    with wire_lock:
        emit({"op": "exec", "command": command, "cwd": cwd, "timeout_sec": timeout})
        reply = sys.stdin.readline()
    if not reply:
        raise RuntimeError("hbench bridge closed")
    r = json.loads(reply)
    if r.get("error"):
        raise RuntimeError(r["error"])
    return r.get("return_code", 0), r.get("stdout") or None, r.get("stderr") or None


def describe(res, if_empty="(no output)"):
    code, out, err = res
    lines = [out] if out else []
    if err:
        lines.append(f"[stderr] {err}")
    if code != 0:
        lines.append(f"[exit code {code}]")
    return "\n".join(lines) or if_empty


def sq(text):
    """Body of a single-quoted shell string."""
    return text.replace("'", "'\\''")


def make_tools(workdir, timeout):
    # Names, signatures and docstrings are what the model sees; keep them stable.
    def sh(command, cwd=None):
        return remote(command, cwd or workdir, timeout)

    def exec_command(command: str, cwd: str | None = None) -> str:
        """Execute a shell command in the environment. Returns stdout+stderr."""
        return describe(sh(command, cwd))

    def read_file(path: str) -> str:
        """Read a file from the environment. Returns file contents."""
        code, out, err = sh(f"cat {shlex.quote(path)}")
        return f"[error] {err or 'file not found'}" if code else out or ""

    def write_file(path: str, content: str) -> str:
        """Write content to a file in the environment."""
        p = shlex.quote(path)
        code, _, err = sh(f'mkdir -p "$(dirname {p})" && ' f"printf '%s' '{sq(content)}' > {p}")
        return f"[error] {err or 'write failed'}" if code else "ok"

    def list_directory(path: str = ".") -> str:
        """List files and directories. Returns ls -la output."""
        code, out, err = sh(f"ls -la {shlex.quote(path)}")
        return f"[error] {err or 'directory not found'}" if code else out or ""

    def find_files(pattern: str, path: str = ".") -> str:
        """Find files matching a glob pattern."""
        code, out, err = sh(f"find {shlex.quote(path)} -name {shlex.quote(pattern)} -type f 2>/dev/null | head -50")
        return f"[error] {err or 'find failed'}" if code else out or "(no matches)"

    def search_content(pattern: str, path: str = ".", file_glob: str = "") -> str:
        """Search file contents with grep. Returns matching lines."""
        include = f"--include={shlex.quote(file_glob)}" if file_glob else ""
        code, out, _ = sh(f"grep -rn {include} {shlex.quote(pattern)} {shlex.quote(path)} 2>/dev/null | head -100")
        return "(no matches)" if code else out or "(no matches)"

    def apply_patch(patch: str) -> str:
        """Apply a unified diff patch. The patch should be in unified diff format."""
        return describe(sh(f"printf '%s' '{sq(patch)}' | patch -p1 --no-backup-if-mismatch"), "patch applied successfully")

    return [exec_command, read_file, write_file, list_directory, find_files, search_content, apply_patch]


def dump_outputs(pred, logs_dir):
    """Write solution / trajectory / final reasoning under <logs>/rlm."""
    d = os.path.join(logs_dir, "rlm")
    os.makedirs(d, exist_ok=True)

    def put(name, text):
        with open(os.path.join(d, name), "w") as f:
            f.write(text)

    fields = list(pred.keys())
    put("solution.txt", str(pred[fields[0]]) if fields else str(pred))
    steps = getattr(pred, "trajectory", None)
    if steps:
        put("trajectory.json", json.dumps(steps, indent=2, default=str))
    reasoning = getattr(pred, "final_reasoning", None)
    if reasoning:
        put("final_reasoning.txt", str(reasoning))


def metrics(pred, lm):
    """Token, cost and step counts for the done message (best effort)."""
    m = {}
    if pred is None:
        return m
    try:
        per_model = pred.get_lm_usage()
        if per_model:
            m["n_input_tokens"] = sum(u.get("input_tokens", 0) for u in per_model.values())
            m["n_output_tokens"] = sum(u.get("output_tokens", 0) for u in per_model.values())
    except (AttributeError, TypeError):
        pass
    try:
        spent = sum(h.get("cost", 0) or 0 for h in lm.history if isinstance(h, dict))
        if spent > 0:
            m["cost_usd"] = spent
    except (AttributeError, TypeError):
        pass
    steps = getattr(pred, "trajectory", None)
    if steps:
        m["rlm_trajectory_steps"] = len(steps)
    return m


def version_probe():
    try:
        import dspy

        emit({"op": "done", "version": dspy.__version__})
    except Exception:
        emit({"op": "done"})


def solve(cfg):
    try:
        import dspy
    except ImportError:
        emit({"op": "done", "kind": "DspyImportError", "error": "dspy is required for the dspy-rlm agent. Install it with: pip install dspy"})
        return
    lm = pred = None
    try:
        tools = make_tools(cfg["working_dir"], cfg["tool_timeout_sec"])
        lm = dspy.LM(cfg["model_name"], max_tokens=16_000)
        helper = cfg.get("sub_model_name")
        agent = dspy.RLM(
            signature=cfg["signature"],
            max_iterations=cfg["max_iterations"],
            max_llm_calls=cfg["max_llm_calls"],
            max_output_chars=cfg["max_output_chars"],
            verbose=cfg["verbose"],
            tools=tools,
            sub_lm=dspy.LM(helper, max_tokens=8_000) if helper else None,
        )
        dspy.configure(lm=lm, track_usage=True)
        pred = agent(**cfg["input_kwargs"])
        dump_outputs(pred, cfg["logs_dir"])
        emit({"op": "done", **metrics(pred, lm)})
    except Exception as e:
        emit({"op": "done", "kind": type(e).__name__, "error": str(e), **metrics(pred, lm)})


if __name__ == "__main__":
    first = json.loads(sys.stdin.readline())
    if first.get("op") == "version":
        version_probe()
    else:
        solve(first)
