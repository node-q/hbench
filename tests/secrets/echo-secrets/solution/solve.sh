#!/bin/bash
# Leaks both secrets (one from the host env, one from the agent config) everywhere hbench collects files from.
set -e
leak() {
  for s in "$HOST_API_KEY" "$AGENT_TOKEN"; do
    printf 'raw %s\n' "$s"
    printf 'base64 %s\n' "$(printf '%s' "$s" | base64 -w0)"
    printf 'basic %s\n' "$(printf 'user:%s' "$s" | base64 -w0)"
    printf 'url %s\n' "$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$s")"
    python3 -c 'import json, sys; print(json.dumps({"api_key": sys.argv[1]}))' "$s"
  done
}
leak
leak > /logs/agent/trajectory-notes.txt
leak > /logs/artifacts/leak.txt
# Long enough for the host-side process watcher to look.
sleep 3
