#!/usr/bin/env bash
# End-to-end check against a real docker daemon: runs the fixture suite with the
# oracle agent and asserts each trial's outcome, then checks no containers leak.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
BIN=target/release/hbench
JOBS=$(mktemp -d)

$BIN run -p tests/fixtures -a oracle -n 4 -k 2 --jobs-dir "$JOBS" --job-name fixtures

python3 - "$JOBS/fixtures" <<'EOF'
import json, sys, pathlib
job = pathlib.Path(sys.argv[1])
expect = {
    "reward-json":     ({"acc": 1, "partial": 0.5}, None),
    "agent-timeout":   ({"reward": 0.0}, "AgentTimeoutError"),
    "agent-outlives-timeout": ({"reward": 1.0}, "AgentTimeoutError"),
    "no-reward":       (None, "RewardFileNotFoundError"),
    "users-env":       ({"reward": 1.0}, None),
    "no-network":      ({"reward": 1.0}, None),
    "compose-sidecar": ({"reward": 1.0}, None),
    "prebuilt":        ({"reward": 1.0}, None),
}
seen = {}
for r in job.glob("*/result.json"):
    d = json.loads(r.read_text())
    task = d["task_name"]
    rewards = (d["verifier_result"] or {}).get("rewards")
    exc = (d["exception_info"] or {}).get("exception_type")
    want_r, want_e = expect[task]
    assert (rewards, exc) == (want_r, want_e), f"{task}: got {rewards} {exc}, want {want_r} {want_e}"
    seen[task] = seen.get(task, 0) + 1
assert seen == {t: 2 for t in expect}, seen
res = json.loads((job / "result.json").read_text())
s = res["stats"]
assert res["finished_at"] and s["n_completed_trials"] == 16 and s["n_errored_trials"] == 6, s
ev = s["evals"]["oracle__fixtures"]
assert ev["exception_stats"].keys() == {"AgentTimeoutError", "RewardFileNotFoundError"}, ev
print("fixture assertions OK")
EOF

left=$(docker ps -a --filter label=com.docker.compose.service=main --format '{{.Names}}' | grep -E '__[0-9a-f]{7}-main' || true)
[ -z "$left" ] || { echo "leaked containers: $left"; exit 1; }
echo "E2E OK ($JOBS)"
