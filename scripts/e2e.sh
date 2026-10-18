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
    "agent-leaves-server": ({"reward": 1.0}, None),
    "server-outlives-timeout": ({"reward": 1.0}, "AgentTimeoutError"),
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
assert res["finished_at"] and s["n_completed_trials"] == 20 and s["n_errored_trials"] == 8, s
ev = s["evals"]["oracle__fixtures"]
assert ev["exception_stats"].keys() == {"AgentTimeoutError", "RewardFileNotFoundError"}, ev
print("fixture assertions OK")
EOF

# Verifier-only files datasets build into the agent's image are gone before a (non-oracle) agent starts.
$BIN run -p tests/integrity -a nop --jobs-dir "$JOBS" --job-name integrity >/dev/null
python3 - "$JOBS/integrity" <<'EOF'
import json, sys, pathlib
got = {}
for r in pathlib.Path(sys.argv[1]).glob("*/result.json"):
    d = json.loads(r.read_text())
    got[d["task_name"]] = (d["verifier_result"] or {}).get("rewards")
assert got == {"hidden-tests": {"reward": 1.0}, "encrypted-solution": {"reward": 1.0}}, got
print("integrity assertions OK")
EOF

# A public task under a run-level allowlist: upstream ignores the hosts, enforce_allowlist applies them.
# Allowlists need nftables fib support in the daemon's kernel (Docker Desktop's lacks it).
nft='[ ! -f /proc/config.gz ] || zcat /proc/config.gz | grep -qE "^CONFIG_NFT_FIB_INET=[ym]"'
if docker container run --rm alpine:3.23 sh -c "$nft"; then
  for enforce in false true; do
    cat > "$JOBS/egress-$enforce.yaml" <<EOF
jobs_dir: $JOBS
job_name: egress-$enforce
tasks: [{path: tests/egress/public-task}]
agents: [{name: oracle, extra_allowed_hosts: [pypi.org], enforce_allowlist: $enforce}]
EOF
    $BIN run -c "$JOBS/egress-$enforce.yaml" >/dev/null
  done
  python3 - "$JOBS" <<'EOF'
import json, sys, pathlib
for enforce, want in [("false", 0.0), ("true", 1.0)]:
    (r,) = pathlib.Path(sys.argv[1], f"egress-{enforce}").glob("*/result.json")
    got = json.loads(r.read_text())["verifier_result"]["rewards"]["reward"]
    assert got == want, f"enforce_allowlist={enforce}: reward {got}, want {want}"
print("egress assertions OK")
EOF
else
  echo "egress assertions skipped: no nftables fib support on this docker host"
fi

left=$(docker ps -a --filter label=com.docker.compose.service=main --format '{{.Names}}' | grep -E '__[0-9a-f]{7}-main' || true)
[ -z "$left" ] || { echo "leaked containers: $left"; exit 1; }
echo "E2E OK ($JOBS)"
