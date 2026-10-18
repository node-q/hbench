#!/bin/bash
# Run with extra_allowed_hosts [pypi.org] and enforce_allowlist: only pypi.org may answer.
cat /app/net.txt
if grep -qx "example.com blocked" /app/net.txt && grep -qx "pypi.org reachable" /app/net.txt; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
