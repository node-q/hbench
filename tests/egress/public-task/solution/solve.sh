#!/bin/bash
# A bare TCP connect can succeed under the egress proxy; only a real HTTP exchange proves reachability.
probe() {
  if timeout 8 bash -c "exec 3<>/dev/tcp/$1/80; printf 'GET / HTTP/1.0\r\nHost: $1\r\n\r\n' >&3; head -c 4 <&3" 2>/dev/null | grep -q HTTP
  then echo "$1 reachable"; else echo "$1 blocked"; fi
}
{ probe example.com; probe pypi.org; } > net.txt
