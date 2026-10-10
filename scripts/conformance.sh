#!/usr/bin/env bash
# Conformance: run upstream's own example tasks under hbench and assert the same
# outcomes upstream's runtime tests expect (tests/runtime/test_network_policy_matrix_docker.py,
# multi-step / separate-verifier / sidecar examples).
#   HBENCH=/path/to/hbench scripts/conformance.sh [suite...]   suites: core matrix extra env
set -euo pipefail
cd "$(dirname "$0")/.."
HBENCH=${HBENCH:-/tmp/hb-ref/hbench}
EX=$HBENCH/examples/tasks
cargo build --release -q
BIN=$PWD/target/release/hbench
OUT=${OUT:-$(mktemp -d)}
SUITES=${*:-core matrix extra env}
N=${N:-6}

job() { # name agent task... -> writes job yaml and runs it
  local name=$1 agent=$2; shift 2
  { echo "jobs_dir: $OUT"; echo "job_name: $name"; echo "n_concurrent_trials: $N"
    echo "agents: [{name: $agent}]"; echo "tasks:"; for t in "$@"; do echo "  - path: $EX/$t"; done; } > "$OUT/$name.yaml"
  "$BIN" run -c "$OUT/$name.yaml" ${EXTRA_FLAGS:-} | tail -n 20
}

check() { # job_dir expected_reward(or 'any1') -> every trial: no exception, reward as expected
  python3 - "$1" "$2" <<'EOF'
import json, sys, pathlib
job, want = pathlib.Path(sys.argv[1]), sys.argv[2]
bad, n = [], 0
for r in sorted(job.glob("*/result.json")):
    d = json.loads(r.read_text()); n += 1
    rw = (d.get("verifier_result") or {}).get("rewards")
    exc = (d.get("exception_info") or {}).get("exception_type")
    ok = exc is None and rw is not None and (
        all(float(v) == 1.0 for v in rw.values()) if want == "all1" else float(rw.get("reward", -1)) == float(want))
    if not ok:
        bad.append(f"{d['task_name']}: rewards={rw} exception={exc} ({r.parent.name})")
print(f"{job.name}: {n - len(bad)}/{n} as expected")
for b in bad: print("  FAIL", b)
sys.exit(1 if bad or n == 0 else 0)
EOF
}

rc=0
for s in $SUITES; do case $s in
  core)
    job core oracle hello-world hello-alpine hello-workdir hello-user hello-healthcheck hello-skills hello-mcp \
      hello-multi-step-simple hello-multi-step-advanced separate-verifier-environment separate-verifier-multi-step \
      sidecar-artifacts verifier-mode-matrix/{multistep-all-separate,multistep-all-shared,multistep-top-separate-mixed,multistep-top-shared-mixed,separate-explicit,separate-implicit,separate-reuse-env,shared-default}
    check "$OUT/core" 1.0 || rc=1 ;;
  matrix)
    job matrix oracle network-policy-matrix/static/{compose-sidecar-controlled,compose-sidecar-uncontrolled-network,e,e-default,e-allowlist,e-allowlist-ipv4-cidr,e-a-v-same,e-ve,e-ve-no-network,e-sa-same,sv-sve-same,verifier-separate-mode} \
      network-policy-matrix/dynamic/{e-a-diff,e-v-diff,e-a-diff-v-match,v-ve-diff,shared-allowlist,e-ve-sve-diff,e-ve-sa-sv-diff,sa-sv-diff,sv-sve-diff,steps-mixed}
    check "$OUT/matrix" 1.0 || rc=1 ;;
  extra)
    job extra-blocked oracle network-policy-matrix/extra-allowed-hosts/{a-allow-agent-host,e-allow-agent-host}
    check "$OUT/extra-blocked" 0.0 || rc=1
    { echo "jobs_dir: $OUT"; echo "job_name: extra-allowed"; echo "agents: [{name: oracle, extra_allowed_hosts: [www.iana.org]}]"; echo "tasks:"
      for t in a-allow-agent-host e-allow-agent-host; do echo "  - path: $EX/network-policy-matrix/extra-allowed-hosts/$t"; done; } > "$OUT/extra-allowed.yaml"
    "$BIN" run -c "$OUT/extra-allowed.yaml" | tail -n 8
    check "$OUT/extra-allowed" 1.0 || rc=1 ;;
  env)
    job env nop environment-env-single environment-env-multi
    check "$OUT/env" 1.0 || rc=1 ;;
esac; done
echo "results in $OUT"
exit $rc
