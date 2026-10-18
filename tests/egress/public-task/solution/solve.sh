#!/bin/bash
# Run under an allowlist of pypi.org, *.python.org and the model host ($MODEL_PROBE, host:port).
# Bash only (the image has no curl). A bare TCP connect can succeed under the egress proxy;
# only a real HTTP exchange proves reachability.
probe() {
  local host=$1 port=$2 via_host=$1 via_port=$2 target=/ status
  if [ -n "${http_proxy:-}" ]; then
    local hp=${http_proxy#http://}
    hp=${hp%/}
    via_host=${hp%:*} via_port=${hp##*:} target="http://$host:$port/"
  fi
  status=$(timeout 8 bash -c "exec 3<>/dev/tcp/$via_host/$via_port; printf 'GET $target HTTP/1.0\r\nHost: $host\r\n\r\n' >&3; head -n1 <&3" 2>/dev/null | awk '{print $2}')
  case "$status" in '' | 403) echo "$host blocked" ;; *) echo "$host reachable" ;; esac
}
# A DNS query over TCP straight to 1.1.1.1, past any proxy: no answer means it was blocked.
dns() {
  if timeout 8 bash -c 'exec 3<>/dev/tcp/1.1.1.1/53; printf "\x00\x1d\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01" >&3; head -c 2 <&3 | od -An -tx1' 2>/dev/null | grep -q '[0-9a-f]'
  then echo "dns reachable"; else echo "dns blocked"; fi
}
{
  probe example.com 80
  probe pypi.org 80
  probe www.python.org 80
  probe "${MODEL_PROBE%:*}" "${MODEL_PROBE##*:}"
  dns
} > net.txt
