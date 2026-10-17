# Run in the container via `python3 -c`: copy OpenClaw's session transcript next to its log.
# The CLI's --json envelope is the trailing JSON object in openclaw.txt; it names the session file.
import json
import shutil
from pathlib import Path

LOG_DIR = Path("/logs/agent")


def trailing_object(text):
    decoder = json.JSONDecoder()
    pos = text.rfind("{")
    while pos >= 0:
        try:
            value, end = decoder.raw_decode(text, pos)
        except ValueError:
            value, end = None, 0
        if isinstance(value, dict) and not text[end:].strip():
            return value
        pos = text.rfind("{", 0, pos)
    return None


def session_file():
    log = LOG_DIR / "openclaw.txt"
    if not log.is_file():
        return None
    envelope = trailing_object(log.read_text(encoding="utf-8", errors="replace").strip()) or {}
    meta = envelope.get("meta")
    agent = meta.get("agentMeta") if isinstance(meta, dict) else None
    path = agent.get("sessionFile") if isinstance(agent, dict) else None
    return Path(path) if isinstance(path, str) and path.strip() else None


src = session_file()
if src is not None and src.is_file():
    shutil.copy2(src, LOG_DIR / "openclaw.session.jsonl")
