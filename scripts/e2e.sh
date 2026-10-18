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

# A public task under a run-level allowlist: the hosts are ignored unless enforce_allowlist applies them.
# The agent phase must reach the allowlisted hosts and a stand-in for the model server on the host,
# and nothing else, under the default method (nft+proxy wherever the kernel takes the nftables rules,
# Docker Desktop included; E2E_REQUIRE_NFT insists on it) and under the proxy-only fallback.
if docker info --format '{{.OperatingSystem}}' | grep -q 'Docker Desktop'; then model_host=host.docker.internal; else model_host=172.17.0.1; fi
model_port=18080
python3 -m http.server "$model_port" --bind 0.0.0.0 --directory "$JOBS" >/dev/null 2>&1 &
model_pid=$!
trap 'kill $model_pid 2>/dev/null' EXIT
default_method=${E2E_REQUIRE_NFT:+nft+proxy}
runs="false true true:proxy"
for run in $runs; do
  enforce=${run%%:*} method=${run#*:}
  [ "$method" = "$run" ] && method=
  cat > "$JOBS/egress-$enforce$method.yaml" <<EOF
jobs_dir: $JOBS
job_name: egress-$enforce$method
tasks: [{path: tests/egress/public-task}]
environment: {env: {MODEL_PROBE: "$model_host:$model_port"}}
agents: [{name: oracle, extra_allowed_hosts: [pypi.org, "*.python.org", $model_host], enforce_allowlist: $enforce}]
EOF
  HBENCH_EGRESS_METHOD=$method $BIN run -c "$JOBS/egress-$enforce$method.yaml" >/dev/null
done
python3 - "$JOBS" "$default_method" $runs <<'EOF'
import json, sys, pathlib
jobs, default = sys.argv[1], sys.argv[2]
for run in sys.argv[3:]:
    enforce, _, method = run.partition(":")
    (r,) = pathlib.Path(jobs, f"egress-{enforce}{method}").glob("*/result.json")
    d = json.loads(r.read_text())
    got = (d["verifier_result"] or {}).get("rewards", {}).get("reward")
    egress = d.get("egress")
    if enforce == "false":
        assert got == 0.0 and egress is None, f"{run}: reward {got}, egress {egress}, want 0.0 and no egress control"
        continue
    want = method or default or egress["method"]
    stdout = next(r.parent.glob("verifier/test-stdout.txt"), None)
    detail = stdout.read_text() if stdout else d["exception_info"]
    assert got == 1.0, f"{run}: reward {got}, want 1.0\n{detail}"
    assert egress["enforced"] and egress["method"] == want and want in ("nft+proxy", "proxy"), f"{run}: egress {egress}, want {want}"
    assert "example.com:80" in egress["denied"], f"{run}: denials {egress['denied']}"
print("egress assertions OK")
EOF

left=$(docker ps -a --filter label=com.docker.compose.service=main --format '{{.Names}}' | grep -E '__[0-9a-f]{7}-main' || true)
[ -z "$left" ] || { echo "leaked containers: $left"; exit 1; }
echo "E2E OK ($JOBS)"
