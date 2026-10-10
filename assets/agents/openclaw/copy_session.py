def _openclaw_container_copy_session_transcript() -> None:
    """
    Stdlib-only logic run inside the agent container ("python3 -c").
    Serialized via "inspect.getsource" as a **single** self-contained function.
    Parse "openclaw.txt" by finding the last JSON object that consumes the file suffix,
    then copy "agentMeta.sessionFile".
    """
    import json
    import shutil
    import sys
    from pathlib import Path

    log_path = Path("/logs/agent/openclaw.txt")
    if not log_path.is_file():
        sys.exit(0)
    raw = log_path.read_text(encoding="utf-8", errors="replace")
    text = raw.strip()
    if not text:
        sys.exit(0)
    dec = json.JSONDecoder()
    envelope = None
    for start in range(len(text) - 1, -1, -1):
        if text[start] != "{":
            continue
        try:
            obj, consumed = dec.raw_decode(text[start:])
        except (json.JSONDecodeError, ValueError):
            continue
        if not isinstance(obj, dict):
            continue
        if text[start + consumed :].strip():
            continue
        envelope = obj
        break
    if not envelope:
        sys.exit(0)
    meta = envelope.get("meta")
    if not isinstance(meta, dict):
        sys.exit(0)
    agent_meta = meta.get("agentMeta")
    if not isinstance(agent_meta, dict):
        sys.exit(0)
    session_file = agent_meta.get("sessionFile")
    if not isinstance(session_file, str) or not session_file.strip():
        sys.exit(0)
    src = Path(session_file)
    if not src.is_file():
        sys.exit(0)
    dst = Path("/logs/agent") / "openclaw.session.jsonl"
    shutil.copy2(src, dst)

_openclaw_container_copy_session_transcript()
