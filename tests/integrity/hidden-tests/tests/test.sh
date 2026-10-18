#!/bin/bash
# The agent's copy keeps the public cases and loses the hidden ones; the verifier's is whole.
python3 - <<'PY' && echo 1 > /logs/verifier/reward.txt || echo 0 > /logs/verifier/reward.txt
import json
agent, verifier = json.load(open("/app/config.json")), json.load(open("/tests/config.json"))
assert "private_test_cases" not in agent and agent["public_test_cases"], agent
assert verifier["private_test_cases"] == "hidden", verifier
PY
