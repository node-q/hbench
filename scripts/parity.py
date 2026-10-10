#!/usr/bin/env python3
"""Compare a upstream job dir with an hbench job dir trial-by-trial (matched by task name).

usage: scripts/parity.py <hbench_job_dir> <hbench_job_dir>
"""
import json, sys, pathlib

def load(job):
    out = {}
    for r in sorted(pathlib.Path(job).glob("*/result.json")):
        d = json.loads(r.read_text())
        out.setdefault(d["task_name"], []).append((r.parent, d))
    return out

def keys(v, prefix=""):
    if isinstance(v, dict):
        s = set()
        for k, x in v.items():
            s.add(prefix + k)
            if k not in ("config", "metadata", "rewards", "task_id", "model_usage"):
                s |= keys(x, prefix + k + ".")
        return s
    if isinstance(v, list) and v and isinstance(v[0], dict):
        return keys(v[0], prefix + "[]")
    return set()

def files(trial):
    skip = {"compose", "lock.json"}
    return {str(p.relative_to(trial)) for p in trial.rglob("*") if p.is_file() and not any(part.startswith("compose") for part in p.relative_to(trial).parts) and p.name not in skip}

def manifest(trial):
    out = []
    for m in sorted(trial.rglob("manifest.json")):
        try:
            for e in json.loads(m.read_text()):
                out.append((str(m.parent.relative_to(trial)), e.get("source"), e.get("destination"), e.get("type"), e.get("status"), e.get("service")))
        except Exception:
            pass
    return sorted(out, key=str)

h, b = load(sys.argv[1]), load(sys.argv[2])
problems = 0
for task in sorted(set(h) | set(b)):
    if task not in h or task not in b:
        print(f"[{task}] only in {'hbench' if task in h else 'hbench'}"); problems += 1; continue
    (ht, hd), (bt, bd) = h[task][0], b[task][0]
    issues = []
    hr, br = (hd.get("verifier_result") or {}).get("rewards"), (bd.get("verifier_result") or {}).get("rewards")
    if hr != br: issues.append(f"rewards hbench={hr} hbench={br}")
    he, be = (hd.get("exception_info") or {}).get("exception_type"), (bd.get("exception_info") or {}).get("exception_type")
    if he != be: issues.append(f"exception hbench={he} hbench={be}")
    for f in ("verifier_environment_mode",):
        if hd.get(f) != bd.get(f): issues.append(f"{f} hbench={hd.get(f)} hbench={bd.get(f)}")
    hs = [s["step_name"] for s in hd.get("step_results") or []]; bs = [s["step_name"] for s in bd.get("step_results") or []]
    if hs != bs: issues.append(f"steps hbench={hs} hbench={bs}")
    for i, (x, y) in enumerate(zip(hd.get("step_results") or [], bd.get("step_results") or [])):
        if (x.get("verifier_result") or {}).get("rewards") != (y.get("verifier_result") or {}).get("rewards"):
            issues.append(f"step {i} rewards hbench={x.get('verifier_result')} hbench={y.get('verifier_result')}")
    kh, kb = keys(hd), keys(bd)
    if kh - kb: issues.append(f"result keys only in hbench: {sorted(kh - kb)}")
    if kb - kh: issues.append(f"result keys only in hbench: {sorted(kb - kh)}")
    fh, fb = files(ht), files(bt)
    if fh - fb: issues.append(f"files only in hbench: {sorted(fh - fb)}")
    if fb - fh: issues.append(f"files only in hbench: {sorted(fb - fh)}")
    mh, mb = manifest(ht), manifest(bt)
    if mh != mb: issues.append(f"manifest hbench={mh}\n      hbench={mb}")
    print(f"[{task}] {'OK' if not issues else 'DIFF'}")
    for i in issues: print("    " + i)
    problems += bool(issues)
print(f"{problems} task(s) differ")
sys.exit(1 if problems else 0)
