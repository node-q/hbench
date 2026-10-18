#!/bin/bash
# The agent phase (solve.sh) ran under the allowlist; the verifier runs with the task's public network.
echo "agent phase:"
cat /app/net.txt
want="example.com blocked
pypi.org reachable
www.python.org reachable
${MODEL_PROBE%:*} reachable
dns blocked"
# Public again, without the proxy: the direct route is back.
direct=$(timeout 8 bash -c 'exec 3<>/dev/tcp/example.com/80; printf "GET / HTTP/1.0\r\nHost: example.com\r\n\r\n" >&3; head -c 4 <&3' 2>/dev/null)
echo "verifier phase: example.com direct: ${direct:-blocked}"
if [ "$(cat /app/net.txt)" = "$want" ] && [ "$direct" = HTTP ]; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
