#!/usr/bin/env bash
# End-to-end check against a real docker daemon. First the fixture suite with the oracle agent, asserting
# each trial's outcome, then the egress and secrets runs, then every other command of the CLI against the
# job dirs those runs left, the example tasks, and small inputs made here (an OCI layout, a registry
# container, a git checkout, a legacy task). Last, a check that no containers leak.
#
# Not covered: `hbench update` (it installs the latest release over the binary under test) and the
# interactive paths (the `run` wizard, `trials handoff` into a real agent CLI, `tasks start-env` with a shell).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build -q
BIN=$PWD/target/debug/hbench
# E2E_JOBS_DIR keeps the job dirs somewhere known (CI uploads it when a run fails).
JOBS=${E2E_JOBS_DIR:-$(mktemp -d)}
mkdir -p "$JOBS"
# No release check over the network, no ANSI colours in the outputs the assertions read.
export HBENCH_NO_UPDATE_CHECK=1 NO_COLOR=1

pids=() containers=()
cleanup() {
  rc=$?
  kill ${pids[@]+"${pids[@]}"} 2>/dev/null || true
  [ ${#containers[@]} = 0 ] || docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
  [ $rc = 0 ] || echo "e2e failed (exit $rc); job dirs: $JOBS" >&2
}
trap cleanup EXIT

# check <text>: stdin must contain the text; on a miss, print what came instead.
check() {
  local out
  out=$(cat)
  grep -qF -- "$1" <<<"$out" || { echo "expected $(printf %q "$1") in:" >&2; echo "$out" >&2; exit 1; }
}
# expect_fail <text> <command...>: the command must exit non-zero and print the text (stdout or stderr).
expect_fail() {
  local want=$1 out
  shift
  if out=$("$@" 2>&1); then echo "expected a failure from: $*" >&2; echo "$out" >&2; exit 1; fi
  grep -qF -- "$want" <<<"$out" || { echo "expected $(printf %q "$want") from: $*" >&2; echo "$out" >&2; exit 1; }
}
# rewards <job dir>: one `task_name rewards exception` line per trial, sorted.
rewards() {
  python3 - "$1" <<'EOF'
import json, sys, pathlib
for r in sorted(pathlib.Path(sys.argv[1]).glob("*/result.json")):
    d = json.loads(r.read_text())
    print(d["task_name"], (d["verifier_result"] or {}).get("rewards"), (d["exception_info"] or {}).get("exception_type"))
EOF
}
# wait_for <url>: poll until it answers, or fail.
wait_for() {
  for _ in $(seq 100); do curl -fs -o /dev/null "$1" && return 0; sleep 0.1; done
  echo "$1 never came up" >&2
  return 1
}

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
    "planted-reward":  ({"reward": 1.0}, None),
}
def explain(trial):
    """What a failed trial left behind: its exception, the end of its log, and the verifier's output."""
    out = []
    for name in ("exception.txt", "trial.log", "verifier/test-stdout.txt"):
        p = trial / name
        if p.exists():
            out.append(f"--- {name} (tail)\n" + "\n".join(p.read_text(errors="replace").splitlines()[-30:]))
    return "\n".join(out)
seen, bad = {}, []
for r in sorted(job.glob("*/result.json")):
    d = json.loads(r.read_text())
    task = d["task_name"]
    rewards = (d["verifier_result"] or {}).get("rewards")
    exc = (d["exception_info"] or {}).get("exception_type")
    want_r, want_e = expect[task]
    if (rewards, exc) != (want_r, want_e):
        bad.append(f"{r.parent.name}: got {rewards} {exc}, want {want_r} {want_e}\n{explain(r.parent)}")
    seen[task] = seen.get(task, 0) + 1
assert not bad, "\n\n".join(bad)
assert seen == {t: 2 for t in expect}, seen
res = json.loads((job / "result.json").read_text())
s = res["stats"]
assert res["finished_at"] and s["n_completed_trials"] == 22 and s["n_errored_trials"] == 8, s
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
# An empty dir: the stand-in must not serve the job dirs to whoever can reach the port.
mkdir -p "$JOBS/model-standin"
python3 -m http.server "$model_port" --bind 0.0.0.0 --directory "$JOBS/model-standin" >"$JOBS/model-standin.log" 2>&1 &
pids+=($!)
# A stand-in that never came up would read as blocked, not as a pass, but fail here with the reason.
wait_for "http://127.0.0.1:$model_port/" || { cat "$JOBS/model-standin.log"; exit 1; }
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

# Secrets from the host env and from the agent config never reach a command line (on the host, where
# a Linux daemon's container processes are visible too), and every common encoding of them is scrubbed
# from the job's files.
host_key="hb-e2e-host-$(openssl rand -hex 12)" agent_token="hb-e2e-agent-$(openssl rand -hex 12)"
cat > "$JOBS/secrets.yaml" <<EOF
jobs_dir: $JOBS
job_name: secrets
tasks: [{path: tests/secrets/echo-secrets}]
agents: [{name: oracle, env: {AGENT_TOKEN: "$agent_token"}}]
EOF
# The watcher stops at a file, not a kill: a killed loop can leave a ps running that then sees the
# next command. It also stops if this script exits first ($$ is the script's pid in the subshell). The
# checker takes the secrets from its environment, off its own command line.
( until [ -e "$JOBS/argv-watch.stop" ] || ! kill -0 $$ 2>/dev/null; do ps -Ao args= | grep -F -e "$host_key" -e "$agent_token" | grep -v -e '^grep ' >> "$JOBS/argv-leaks.txt" || true; sleep 0.1; done ) &
watch_pid=$!
HB_E2E_API_KEY=$host_key $BIN run -c "$JOBS/secrets.yaml" >/dev/null
touch "$JOBS/argv-watch.stop"; wait $watch_pid
E2E_SECRETS="$host_key $agent_token" python3 - "$JOBS" <<'EOF'
import base64, json, os, pathlib, sys, urllib.parse
jobs, secrets = pathlib.Path(sys.argv[1]), os.environ["E2E_SECRETS"].split()
argv = jobs / "argv-leaks.txt"
leaks = argv.read_text().splitlines() if argv.exists() else []
assert not leaks, f"secrets on a command line: {leaks}"
(r,) = (jobs / "secrets").glob("*/result.json")
d = json.loads(r.read_text())
assert (d["verifier_result"] or {}).get("rewards") == {"reward": 1.0}, d["exception_info"]
forms = {}
for s in secrets:
    forms |= {f"raw {s}": s, f"base64 {s}": base64.b64encode(s.encode()).decode(), f"url {s}": urllib.parse.quote(s, safe="")}
    # The middle of user:<secret> in base64 (the ends share bits with the neighbours).
    forms[f"basic {s}"] = base64.b64encode(f"user:{s}".encode()).decode()[8:-4]
files = [p for p in (jobs / "secrets").rglob("*") if p.is_file()]
found = [(name, str(p)) for p in files for name, f in forms.items() if f.encode() in p.read_bytes()]
assert not found, f"unredacted: {found}"
names = {p.name for p in files if b"[REDACTED]" in p.read_bytes()}
assert {"oracle.txt", "trajectory-notes.txt", "leak.txt", "test-stdout.txt"} <= names, names
print("secret assertions OK")
EOF

# ------------------------------------------------------------------------------------------------------
# The rest of the CLI. Everything below this line writes under $CLI or into a named job dir under $JOBS.
CLI=$JOBS/cli
mkdir -p "$CLI"

# --- Version, completions, schemas, the agent list ---------------------------------------------------
$BIN --version | check "hbench "
$BIN completions bash | check "_hbench"
$BIN completions zsh | check "#compdef hbench"
$BIN completions fish | check "complete -c hbench"
$BIN agents | check oracle
$BIN agents list | check nop
$BIN agents list --json | python3 -c 'import json, sys; a = json.load(sys.stdin); assert {"oracle", "nop", "claude-code"} <= set(a), a'
$BIN agents schema claude-code | check max_turns
$BIN agents schema claude-code --json | python3 -c 'import json, sys; assert "max_turns" in json.load(sys.stdin)["properties"]'
expect_fail "has no options schema" $BIN agents schema oracle
$BIN tasks schema | python3 -c 'import json, sys; assert "properties" in json.load(sys.stdin)'
$BIN trials schema | python3 -c 'import json, sys; assert "properties" in json.load(sys.stdin)'
$BIN update --help | check "latest release"
echo "offline command assertions OK"

# --- Task authoring: init, check, digest, dataset manifests, a legacy task migrated ------------------
A=$CLI/authoring
$BIN init -t e2e/scaffold -o "$A" --no-pytest --author "E2E <e2e@example.com>" >/dev/null
for f in task.toml instruction.md environment/Dockerfile tests/test.sh solution/solve.sh; do [ -f "$A/scaffold/$f" ] || { echo "init -t left no $f"; exit 1; }; done
$BIN tasks init e2e/scaffold-steps -p "$A" --steps 2 --include-standard-metadata >/dev/null
[ -f "$A/scaffold-steps/steps/step-2/tests/test.sh" ] || { echo "tasks init --steps 2 left no step-2"; exit 1; }
$BIN tasks check -p "$A/scaffold" | check "ok    e2e/scaffold"
$BIN tasks check -p examples/tasks | check "ok    hbench/hello-world"
[ "$($BIN tasks check -p examples/tasks | grep -c '^ok')" = 3 ]
mkdir -p "$CLI/broken/task"; echo 'not = [toml' > "$CLI/broken/task/task.toml"
expect_fail "1 invalid task" $BIN tasks check -p "$CLI/broken"
$BIN tasks digest -p examples/tasks | python3 -c '
import json, sys
rows = json.load(sys.stdin)
assert {r["name"] for r in rows} == {"hbench/hello-world", "hbench/hello-alpine", "hbench/hello-workdir"}, rows
assert all(r["digest"].startswith("sha256:") and len(r["checksum"]) == 64 for r in rows), rows'
$BIN tasks digest -p examples/tasks/hello-world | python3 -c 'import json, sys; (r,) = json.load(sys.stdin); assert r["name"] == "hbench/hello-world"'

$BIN init -d e2e/ds -o "$A/ds" --description "An e2e dataset" >/dev/null
[ -f "$A/ds/dataset.toml" ]
$BIN datasets init e2e/ds2 -o "$A/ds2" --with-metric >/dev/null
[ -f "$A/ds2/metric.py" ] && [ -f "$A/ds2/dataset.toml" ]
$BIN add examples/tasks/hello-world examples/tasks/hello-alpine --to "$A/ds" | check hello-alpine
$BIN add examples/tasks --scan --to "$A/ds" | check "already present"
grep -q 'hbench/hello-workdir' "$A/ds/dataset.toml"
$BIN datasets add "$A/ds" --to "$A/ds2" >/dev/null
grep -q 'hbench/hello-world' "$A/ds2/dataset.toml"
$BIN remove hbench/hello-alpine --from "$A/ds" >/dev/null
$BIN datasets remove examples/tasks/hello-workdir --from "$A/ds" >/dev/null
! grep -q 'hello-alpine\|hello-workdir' "$A/ds/dataset.toml"
# sync restores a digest the manifest got wrong, from the task's copy inside the dataset dir.
cp -r examples/tasks/hello-world "$A/ds/hello-world"
digest=$(grep -o 'sha256:[0-9a-f]*' "$A/ds/dataset.toml" | head -1)
python3 - "$A/ds/dataset.toml" "$digest" <<'EOF'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); p.write_text(p.read_text().replace(sys.argv[2], "sha256:" + "0" * 64))
EOF
$BIN sync "$A/ds" | check "Updated 1 digest(s)"
grep -q "$digest" "$A/ds/dataset.toml"
$BIN sync "$A/ds/dataset.toml" | check "Updated 0 digest(s)"

# A Terminal-Bench v1 task, migrated, then run: the migration has to produce a task the oracle scores 1 on.
L=$CLI/legacy/hello-legacy
mkdir -p "$L"
cat > "$L/task.yaml" <<'EOF'
instruction: |-
  Create a file hello.txt containing "Hello, world!" in /app.
author_name: Jane Doe
author_email: jane@example.com
difficulty: easy
category: file-operations
tags: [file-operations]
parser_name: pytest
max_agent_timeout_sec: 120
max_test_timeout_sec: 60
EOF
printf 'FROM ubuntu:24.04\nWORKDIR /app\n' > "$L/Dockerfile"
cat > "$L/docker-compose.yaml" <<'EOF'
services:
  client:
    build:
      dockerfile: Dockerfile
    image: ${T_BENCH_TASK_DOCKER_CLIENT_IMAGE_NAME}
    container_name: ${T_BENCH_TASK_DOCKER_CLIENT_CONTAINER_NAME}
    command: ["sh", "-c", "sleep infinity"]
    environment:
      - TEST_DIR=${T_BENCH_TEST_DIR}
    volumes:
      - ${T_BENCH_TASK_LOGS_PATH}:${T_BENCH_CONTAINER_LOGS_PATH}
      - ${T_BENCH_TASK_AGENT_LOGS_PATH}:${T_BENCH_CONTAINER_AGENT_LOGS_PATH}
EOF
printf '#!/bin/bash\necho "Hello, world!" > /app/hello.txt\n' > "$L/solution.sh"
printf '#!/bin/bash\ntest "$(cat /app/hello.txt)" = "Hello, world!"\n' > "$L/run-tests.sh"
$BIN tasks migrate -i "$CLI/legacy" -o "$CLI/migrated" | check "Successfully migrated 1 task"
$BIN tasks migrate -i "$L" -o "$CLI/migrated-one" --cpus 2 --memory-mb 1024 | check "Successfully migrated task to"
grep -q 'cpus = 2' "$CLI/migrated-one/hello-legacy/task.toml"
$BIN tasks check -p "$CLI/migrated" | check "ok    "
$BIN run -p "$CLI/migrated/hello-legacy" -a oracle --jobs-dir "$JOBS" --job-name migrated >/dev/null
rewards "$JOBS/migrated" | check "{'reward': 1.0} None"
echo "authoring assertions OK"

# --- Jobs: start, summary, verify, resume, regrade, upload ------------------------------------------
$BIN jobs start -p examples/tasks/hello-alpine -a nop --jobs-dir "$JOBS" --job-name start >/dev/null
rewards "$JOBS/start" | check "hbench/hello-alpine {'reward': 0.0} None"
expect_fail "already exists" $BIN run -p examples/tasks/hello-alpine -a nop --jobs-dir "$JOBS" --job-name start
$BIN jobs summary "$JOBS/fixtures" | check oracle
$BIN jobs summary -p "$JOBS/fixtures" --json | python3 -c 'import json, sys; s = json.load(sys.stdin)["stats"]; assert s["n_completed_trials"] == 22, s'
$BIN jobs verify "$JOBS/fixtures" | check "every file matches its manifest"
cp -r "$JOBS/fixtures" "$CLI/tampered"
echo tampered >> "$(ls -d "$CLI"/tampered/*/ | head -1)trial.log"
expect_fail "differ from the manifest" $BIN jobs verify -p "$CLI/tampered"
# Resume reruns only what is missing: one trial dir removed from a copy of the fixtures job.
cp -r "$JOBS/fixtures" "$CLI/resume"
gone=$(ls -d "$CLI"/resume/users-env__*/ | head -1)
rm -r "$gone"
$BIN jobs resume -p "$CLI/resume" --skip-preflight 2>&1 | check "21 trial(s) already complete, 1 to run"
[ "$(ls -d "$CLI"/resume/users-env__*/ | wc -l)" = 2 ] && [ "$(ls -d "$CLI"/resume/*/ | wc -l)" = 22 ]
python3 -c 'import json, sys; s = json.load(open(sys.argv[1]))["stats"]; assert (s["n_completed_trials"], s["n_errored_trials"]) == (22, 8), s' "$CLI/resume/result.json"
$BIN jobs verify "$CLI/resume" | check "every file matches its manifest"
expect_fail "" $BIN jobs resume -p "$CLI/resume" --fail-on-error
# Regrade needs a separate verifier, which scores collected artifacts; the regrade job scores the source job's again.
T=$CLI/tasks/hello-separate
mkdir -p "$CLI/tasks" && cp -r examples/tasks/hello-world "$T"
python3 - "$T/task.toml" <<'EOF'
import sys, pathlib
p = pathlib.Path(sys.argv[1])
t = p.read_text().replace("[verifier]\n", '[verifier]\nenvironment_mode = "separate"\n')
# The separate verifier sees the collected artifacts at their original paths, and nothing else of /app.
p.write_text(t.replace('schema_version = "1.4"\n', 'schema_version = "1.4"\nartifacts = ["/app/hello.txt"]\n'))
EOF
$BIN run -p "$T" -a oracle --jobs-dir "$JOBS" --job-name regrade-src >/dev/null
rewards "$JOBS/regrade-src" | check "{'reward': 1.0} None"
$BIN jobs regrade --source "$JOBS/regrade-src" -p "$T" --jobs-dir "$JOBS" --job-name regrade -n 1 >/dev/null
rewards "$JOBS/regrade" | check "hbench/hello-world {'reward': 1.0} None"
expect_fail "regrade needs" $BIN jobs regrade --source "$JOBS/start" -p examples/tasks/hello-alpine --jobs-dir "$JOBS"
# Upload: a stand-in server that wants the bearer token and an extra header, and refuses one path.
upload_token="hb-e2e-upload-$(openssl rand -hex 8)"
cat > "$CLI/upload-server.py" <<'EOF'
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
token = os.environ["E2E_UPLOAD_TOKEN"]
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        if self.headers.get("Authorization") != f"Bearer {token}" or self.headers.get("X-E2E") != "yes":
            code, out = 401, {"detail": "bad credentials"}
        elif self.path == "/refuse":
            code, out = 403, {"detail": "not today"}
        elif self.headers.get("Content-Type") != "application/gzip" or body[:2] != b"\x1f\x8b":
            code, out = 400, {"detail": "not a gzip body"}
        else:
            code, out = 200, {"accepted": True, "bytes": len(body)}
        data = json.dumps(out).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(data))); self.end_headers()
        self.wfile.write(data)
srv = HTTPServer(("127.0.0.1", 0), H)
print(srv.server_port, flush=True)
srv.serve_forever()
EOF
E2E_UPLOAD_TOKEN=$upload_token python3 "$CLI/upload-server.py" > "$CLI/upload-port.txt" &
pids+=($!)
for _ in $(seq 50); do [ -s "$CLI/upload-port.txt" ] && break; sleep 0.1; done
upload_url="http://127.0.0.1:$(cat "$CLI/upload-port.txt")"
E2E_UPLOAD_TOKEN=$upload_token $BIN jobs upload "$JOBS/start" --url "$upload_url/ok" --token-env E2E_UPLOAD_TOKEN -H "X-E2E: yes" | check '"accepted": true'
expect_fail "refused the upload (403" env E2E_UPLOAD_TOKEN=$upload_token $BIN jobs upload -p "$JOBS/start" --url "$upload_url/refuse" --token-env E2E_UPLOAD_TOKEN -H "X-E2E: yes"
expect_fail "(401" $BIN jobs upload "$JOBS/start" --url "$upload_url/ok"
expect_fail "E2E_MISSING_TOKEN is not set" $BIN jobs upload "$JOBS/start" --url "$upload_url/ok" --token-env E2E_MISSING_TOKEN
expect_fail "differ from its manifest.json" env E2E_UPLOAD_TOKEN=$upload_token $BIN jobs upload "$CLI/tampered" --url "$upload_url/ok" --token-env E2E_UPLOAD_TOKEN -H "X-E2E: yes"
echo "jobs assertions OK"

# --- Trials: start, handoff -------------------------------------------------------------------------
$BIN trials start -p examples/tasks/hello-world --trials-dir "$CLI/trials" --ae E2E_VAR=1 > "$CLI/trial.json"
python3 -c '
import json, sys
r = json.load(open(sys.argv[1]))
assert r["rewards"] == {"reward": 1.0} and r["exception"] is None and r["trial_uri"].startswith("file://"), r' "$CLI/trial.json"
trial=$(ls -d "$CLI"/trials/*/ | head -1)
expect_fail "does not support handoff" $BIN trials handoff "$trial"
expect_fail "not a trial directory" $BIN trials handoff "$CLI"
expect_fail "pass -p <task dir> or -c <trial.yaml>" $BIN trials start
echo "trials assertions OK"

# --- Traces: an ATIF trajectory planted in a copy of a finished trial ---------------------------------
cp -r "$JOBS/start" "$CLI/traces"
python3 - "$CLI/traces" <<'EOF'
import json, sys, pathlib
(trial,) = [p.parent for p in pathlib.Path(sys.argv[1]).glob("*/result.json")]
(trial / "agent").mkdir(exist_ok=True)
(trial / "agent/trajectory.json").write_text(json.dumps({
    "schema_version": "ATIF-v1.7", "agent": {"name": "nop", "version": "1.0.0", "model_name": "none"},
    "steps": [
        {"step_id": 1, "source": "user", "message": "Make hello.txt"},
        {"step_id": 2, "source": "agent", "message": "Looking", "tool_calls": [{"tool_call_id": "t1", "function_name": "Bash", "arguments": {"command": "ls"}}],
         "observation": {"results": [{"source_call_id": "t1", "content": "a.txt"}]}},
        {"step_id": 3, "source": "agent", "message": "Done."},
    ]}))
EOF
$BIN traces export -p "$CLI/traces" -o "$CLI/traces.jsonl" --sharegpt --instruction-metadata --verifier-metadata 2>&1 | check "Exported 2 rows"
python3 -c '
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
assert len(rows) == 2 and rows[0]["task"] == "hbench/hello-alpine" and rows[0]["result"] == "0.0", rows
assert "conversations_sharegpt" in rows[0] and rows[0]["conversations"][0]["role"] == "user", rows[0]
assert rows[1]["conversations"][-1] == {"role": "assistant", "content": "Done."}, rows[1]' "$CLI/traces.jsonl"
[ "$($BIN traces export -p "$CLI/traces" --episodes last --filter failure 2>/dev/null | wc -l)" = 1 ]
expect_fail "no exportable trials" $BIN traces export -p "$CLI/traces" --filter success
expect_fail "no exportable trials" $BIN traces export -p "$CLI/traces" --no-recursive
echo "traces assertions OK"

# --- Datasets: an OCI layout, a registry container, a registry.json over a git checkout -------------
D=$CLI/datasets
mkdir -p "$D"
cat > "$D/source.yaml" <<'EOF'
schema_version: 1
name: e2e-examples
version: "1.0"
description: Three example tasks.
license: Apache-2.0
EOF
pack_digest=$($BIN datasets pack examples/tasks --descriptor "$D/source.yaml" --oci-layout "$D/layout" 2> "$D/pack.err")
check "packed e2e-examples@1.0: 3 task(s)" < "$D/pack.err"
[[ $pack_digest == sha256:* ]]
$BIN datasets list --registry "oci-layout:$D/layout" | check "e2e-examples@1.0  3 tasks  Three example tasks."
$BIN datasets download e2e-examples@1.0 --registry "oci-layout:$D/layout" -o "$D/dl" > "$D/dl.txt"
[ "$(wc -l < "$D/dl.txt")" = 3 ] && while read -r p; do [ -f "$p/task.toml" ] || { echo "$p has no task.toml"; exit 1; }; done < "$D/dl.txt"
[ "$($BIN datasets export -d e2e-examples@1.0 --registry "oci-layout:$D/layout" --oci-layout "$D/layout-copy" 2>/dev/null)" = "$pack_digest" ]
$BIN datasets list --registry "oci-layout:$D/layout-copy" | check e2e-examples@1.0
$BIN run -d e2e-examples@1.0 --registry "oci-layout:$D/layout" -a oracle -x '*workdir' --jobs-dir "$JOBS" --job-name oci-run >/dev/null
rewards "$JOBS/oci-run" | check "hbench/hello-alpine {'reward': 1.0} None"
[ "$(rewards "$JOBS/oci-run" | wc -l)" = 2 ]
# The lock file: written in the working directory by --lock-write, then insisted on by --locked.
mkdir -p "$D/locked"
(cd "$D/locked" && $BIN datasets download e2e-examples@1.0 --registry "oci-layout:$D/layout" -o "$D/dl-locked" --lock-write >/dev/null)
grep -q "$pack_digest" "$D/locked/hbench.lock"
(cd "$D/locked" && $BIN datasets download e2e-examples@1.0 --registry "oci-layout:$D/layout" -o "$D/dl-locked" --locked >/dev/null)
mkdir -p "$D/unlocked"
(cd "$D/unlocked" && expect_fail "hbench.lock" $BIN datasets download e2e-examples@1.0 --registry "oci-layout:$D/layout" -o "$D/dl-locked" --locked)
# put_index <registry host> [<name> <version> <digest>]...: publish the dataset index a registry is listed from
# (`<namespace>/index:latest`), which `datasets push` leaves to the registry's owner.
put_index() {
  python3 - "$@" <<'EOF'
import hashlib, json, sys, urllib.request
host, triples = sys.argv[1], sys.argv[2:]
entries = [{"name": n, "version": v, "digest": d} for n, v, d in zip(triples[::3], triples[1::3], triples[2::3])]
index = json.dumps({"schema_version": 1, "datasets": entries}).encode()
empty = b"{}"
def digest(b): return "sha256:" + hashlib.sha256(b).hexdigest()
def send(method, url, body, ctype):
    req = urllib.request.Request(url, data=body, method=method, headers={"Content-Type": ctype})
    with urllib.request.urlopen(req) as r: return r.headers
for blob in (index, empty):
    location = send("POST", f"http://{host}/v2/e2e/index/blobs/uploads/", b"", "application/octet-stream")["Location"]
    sep = "&" if "?" in location else "?"
    send("PUT", f"{location}{sep}digest={digest(blob)}", blob, "application/octet-stream")
manifest = json.dumps({
    "schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "artifactType": "application/vnd.hbench.dataset-index.v1",
    "config": {"mediaType": "application/vnd.oci.empty.v1+json", "digest": digest(empty), "size": len(empty)},
    "layers": [{"mediaType": "application/vnd.hbench.dataset-index.v1+json", "digest": digest(index), "size": len(index)}],
}).encode()
send("PUT", f"http://{host}/v2/e2e/index/manifests/latest", manifest, "application/vnd.oci.image.manifest.v1+json")
EOF
}
# A registry: push, list, download, export, and the push again that finds the manifest already there.
registry_image=${E2E_REGISTRY_IMAGE:-registry:3}
registry=$(docker run -d --rm -p 127.0.0.1::5000 "$registry_image")
containers+=("$registry")
registry_host=$(docker port "$registry" 5000/tcp | head -1)
wait_for "http://$registry_host/v2/"
push_digest=$($BIN datasets push examples/tasks --descriptor "$D/source.yaml" --to "oci://$registry_host/e2e/e2e-examples:1.0" 2> "$D/push.err")
check "3 task(s); pushed to" < "$D/push.err"
[[ $push_digest == sha256:* ]]
$BIN datasets push examples/tasks --descriptor "$D/source.yaml" --to "oci://$registry_host/e2e/e2e-examples:1.0" 2>&1 >/dev/null | check "names this manifest already"
expect_fail "the repository's last component is the dataset name" $BIN datasets push examples/tasks --descriptor "$D/source.yaml" --to "oci://$registry_host/e2e/other:1.0"
expect_fail "has no dataset index" $BIN datasets list --registry "oci://$registry_host/e2e"
put_index "$registry_host" e2e-examples 1.0 "$push_digest"
$BIN datasets list --registry "oci://$registry_host/e2e" | check "e2e-examples@1.0  3 tasks"
HBENCH_REGISTRY="oci://$registry_host/e2e" $BIN datasets list | check e2e-examples@1.0
$BIN datasets download e2e-examples@1.0 --registry "oci://$registry_host/e2e" -o "$D/dl-registry" | check hello-workdir
[ "$($BIN datasets export -d e2e-examples@1.0 --registry "oci://$registry_host/e2e" --oci-layout "$D/layout-from-registry" 2>/dev/null)" = "$push_digest" ]
expect_fail "an OCI dataset needs a version" $BIN run -d e2e-examples --registry "oci://$registry_host/e2e" -a oracle --jobs-dir "$JOBS" --job-name registry-run
$BIN run -d e2e-examples@1.0 --registry "oci://$registry_host/e2e" -a oracle -i hello-world --jobs-dir "$JOBS" --job-name registry-run >/dev/null
rewards "$JOBS/registry-run" | check "hbench/hello-world {'reward': 1.0} None"
# A registry.json whose tasks live in a git repository, converted to an OCI layout and pushed.
git init -q "$D/repo" && cp -r examples/tasks "$D/repo/tasks"
git -C "$D/repo" add -A && git -C "$D/repo" -c user.name=e2e -c user.email=e2e@example.com commit -qm "example tasks"
commit=$(git -C "$D/repo" rev-parse HEAD)
python3 - "$D" "$commit" <<'EOF'
import json, sys
d, commit = sys.argv[1], sys.argv[2]
tasks = [{"name": n, "git_url": f"file://{d}/repo", "git_commit_id": commit, "path": f"tasks/{n}"} for n in ("hello-world", "hello-alpine")]
json.dump([{"name": "e2e-git", "version": "1.0", "description": "Two tasks from a git checkout.", "tasks": tasks}], open(f"{d}/registry.json", "w"))
EOF
$BIN datasets list --registry-path "$D/registry.json" | check "e2e-git@1.0  2 tasks  Two tasks from a git checkout."
python3 - "$D" > "$D/http-port.txt" 2>/dev/null <<'EOF' &
import functools, sys
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
srv = ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(SimpleHTTPRequestHandler, directory=sys.argv[1]))
print(srv.server_port, flush=True)
srv.serve_forever()
EOF
pids+=($!)
for _ in $(seq 50); do [ -s "$D/http-port.txt" ] && break; sleep 0.1; done
http_port=$(cat "$D/http-port.txt")
$BIN datasets list --registry-url "http://127.0.0.1:$http_port/registry.json" | check e2e-git@1.0
$BIN datasets download e2e-git@1.0 --registry-path "$D/registry.json" -o "$D/dl-git" | check hello-alpine
$BIN datasets convert "$D/registry.json" -d e2e-git@1.0 --oci-layout "$D/layout-git" --check-equivalence 2> "$D/convert.err" | check "e2e-git@1.0 sha256:"
check "2 task(s), equivalent to the checkouts" < "$D/convert.err"
$BIN datasets list --registry "oci-layout:$D/layout-git" | check "e2e-git@1.0  2 tasks"
sed 's/e2e-examples/e2e-git/; s/Three example tasks./Two tasks, from git./' "$D/source.yaml" > "$D/source-git.yaml"
git_digest=$($BIN datasets convert "http://127.0.0.1:$http_port/registry.json" -d e2e-git@1.0 --to "oci://$registry_host/e2e/e2e-git:1.0" --descriptor "$D/source-git.yaml" | tee "$D/convert-push.txt" | cut -d' ' -f2)
check "e2e-git@1.0 sha256:" < "$D/convert-push.txt"
put_index "$registry_host" e2e-examples 1.0 "$push_digest" e2e-git 1.0 "$git_digest"
$BIN datasets list --registry "oci://$registry_host/e2e" > "$D/list-both.txt"
check "e2e-examples@1.0  3 tasks" < "$D/list-both.txt" && check "e2e-git@1.0       2 tasks  Two tasks, from git." < "$D/list-both.txt"
$BIN datasets download e2e-git@1.0 --registry "oci://$registry_host/e2e" -o "$D/dl-git-registry" | check hello-alpine
$BIN run -d e2e-git@1.0 --registry-path "$D/registry.json" -a oracle -l 1 --jobs-dir "$JOBS" --job-name git-run >/dev/null
[ "$(rewards "$JOBS/git-run" | wc -l)" = 1 ] && rewards "$JOBS/git-run" | check "{'reward': 1.0} None"
echo "dataset assertions OK"

# --- Sweeps: a task that scores and one that never reports `reward`, so it stays for the next sweep --
cat > "$CLI/sweeps.yaml" <<EOF
jobs_dir: $JOBS
job_name: sweeps
tasks: [{path: examples/tasks/hello-world}, {path: tests/fixtures/reward-json}]
agents: [{name: oracle}]
EOF
$BIN sweeps run -c "$CLI/sweeps.yaml" --max-sweeps 2 --trials-per-task 1 --hint "Be quick." > "$CLI/sweeps.txt"
check "$JOBS/sweeps.sweep-2" < "$CLI/sweeps.txt"
[ "$(rewards "$JOBS/sweeps.sweep-1" | wc -l)" = 2 ] && [ "$(rewards "$JOBS/sweeps.sweep-2" | wc -l)" = 1 ]
rewards "$JOBS/sweeps.sweep-2" | check reward-json
echo '{"reward-json": "Report acc and partial."}' > "$CLI/hints.json"
sed 's/job_name: sweeps/job_name: sweeps-hints/' "$CLI/sweeps.yaml" > "$CLI/sweeps-hints.yaml"
$BIN sweeps run -c "$CLI/sweeps-hints.yaml" --max-sweeps 1 --trials-per-task 1 --hints-file "$CLI/hints.json" | check "sweeps-hints.sweep-1.reward-json"
[ -d "$JOBS/sweeps-hints.sweep-1.hello-world" ]
echo "sweeps assertions OK"

# --- Exec: a config printed without docker, then a map job whose template solution writes the artifact
$BIN exec -i "Write hi to /app/out.txt and copy it to /app/data/copy.txt" --print-config | python3 -c '
import json, sys
cfg = json.dumps(json.load(sys.stdin))
assert "/app/out.txt" in cfg and "/app/data/copy.txt" in cfg, cfg'
expect_fail "--config cannot be combined" $BIN exec -c "$CLI/sweeps.yaml" -i "x" --print-config
mkdir -p "$CLI/exec-tpl/solution"
printf '#!/bin/bash\nmkdir -p /app/data\necho hi > /app/out.txt\ncp /app/out.txt /app/data/copy.txt\n' > "$CLI/exec-tpl/solution/solve.sh"
$BIN exec -i "Write hi to /app/out.txt and copy it to /app/data/copy.txt" --task-template "$CLI/exec-tpl" -a oracle --image ubuntu:24.04 \
  --jobs-dir "$JOBS" --job-name exec-map --tasks-dir "$CLI/exec-tasks" -q > "$CLI/exec.txt"
check "Map job written to $JOBS/exec-map" < "$CLI/exec.txt"
rewards "$JOBS/exec-map" | check "{'reward': 1.0} None"
[ -f "$CLI/exec-tasks"/*/instruction.md ]
find "$JOBS/exec-map" -path '*/artifacts/*' -name copy.txt | grep -q copy.txt
echo "exec assertions OK"

# --- Analyze and check: the wrapper jobs run with the nop agent, which writes no verdict, so each report
# records the missing result and the command exits 1.
expect_fail "analysis.json" $BIN analyze "$trial" -a nop -o "$JOBS" --job-name analyze -q
python3 -c '
import json, sys
(r,) = json.load(open(sys.argv[1]))["results"]
assert r["error"], r' "$JOBS/analyze/analysis.json"
expect_fail "Report:" $BIN check examples/tasks -a nop -i 'hello-w*' -l 1 -o "$JOBS" --job-name check -q
[ -f "$JOBS/check/result.json" ]
echo "analyze/check assertions OK"

# --- The environment shell without the shell, and the cache report ---------------------------------
$BIN tasks start-env -p examples/tasks/hello-world --non-interactive 2>&1 | check "starting environment"
$BIN tasks start-env -p examples/tasks/hello-world --non-interactive --no-all --agent nop 2>&1 | check "agent setup complete"
$BIN cache clean --dry | check "Docker images:"
$BIN cache clean --dry --no-docker --no-cache-dir | check "Docker images: 0"
echo "start-env and cache assertions OK"

# --- The viewer: jobs mode on the job dirs, tasks mode on the examples, read-only off loopback ------
view() { # view <log> <args...>: start the viewer, print its base URL.
  local log=$1
  shift
  $BIN view "$@" > "$log" 2>&1 &
  pids+=($!)
  for _ in $(seq 100); do grep -q 'Server: ' "$log" && break; sleep 0.1; done
  grep -o 'Server: http://[^ ]*' "$log" | cut -d' ' -f2 || { cat "$log"; return 1; }
}
url=$(view "$CLI/view-jobs.log" "$JOBS" --port 18090-18099)
curl -fs "$url/api/health" | check '"status":"ok"'
curl -fs "$url/api/config" | check '"mode":"jobs"'
curl -fs "$url/api/jobs?q=fixtures" | check '"fixtures"'
curl -fs "$url/api/jobs/fixtures" | check n_completed_trials
curl -fs "$url/" | check "<html"
url=$(view "$CLI/view-tasks.log" -p examples/tasks --tasks --port 18090-18099)
curl -fs "$url/api/config" | check '"mode":"tasks"'
curl -fs "$url/api/task-definitions" | check hello-world
url=$(view "$CLI/view-readonly.log" "$JOBS" --jobs --host 0.0.0.0 --port 18090-18099)
check "Read-only" < "$CLI/view-readonly.log"
[ "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "${url/0.0.0.0/127.0.0.1}/api/jobs/start")" = 403 ]
[ -d "$JOBS/start" ]
expect_fail "does not exist" $BIN view "$CLI/nowhere"
echo "viewer assertions OK"

# hbench view follows a running trial: the agent's logs reach the trial dir before the agent exits.
cp -R examples/tasks/hello-alpine "$JOBS/live-task"
cat > "$JOBS/live-task/solution/solve.sh" <<'EOF'
#!/bin/sh
echo "Hello, world!" > /app/hello.txt
echo "live $LIVE_KEY" > /logs/agent/live.txt
sleep 20
EOF
live_key="hb-e2e-live-$(openssl rand -hex 12)"
$BIN run -p "$JOBS/live-task" -a oracle --ae "LIVE_KEY=$live_key" --jobs-dir "$JOBS" --job-name live >/dev/null &
live_pid=$!
# Scrubbed before it's served: the raw key never lands in the trial dir, not even for a moment.
live_seen=
while kill -0 $live_pid 2>/dev/null; do
  if compgen -G "$JOBS/live/*/agent/live.txt" >/dev/null; then
    live_seen=1
    grep -F "[REDACTED]" "$JOBS"/live/*/agent/live.txt >/dev/null || { echo "raw agent log reached the host"; exit 1; }
  fi
  sleep 0.1
done
wait $live_pid
[ -n "$live_seen" ] || { echo "agent logs did not reach the host during the run"; exit 1; }
echo "live sync OK"

left=$(docker ps -a --filter label=com.docker.compose.service=main --format '{{.Names}}' | grep -E '__[0-9a-f]{7}-main' || true)
[ -z "$left" ] || { echo "leaked containers: $left"; exit 1; }
echo "E2E OK ($JOBS)"
