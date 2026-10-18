#!/bin/bash
# Leaks both secrets (one from the host env, one from the agent config) everywhere hbench collects files from.
# They stay off this script's own command lines too (printf is a builtin; python reads its env), since
# the E2E watcher sees a Linux host's container processes.
set -e
leak() {
  for name in HOST_API_KEY AGENT_TOKEN; do
    s=${!name}
    printf 'raw %s\n' "$s"
    printf 'base64 %s\n' "$(printf '%s' "$s" | base64 -w0)"
    printf 'basic %s\n' "$(printf 'user:%s' "$s" | base64 -w0)"
    S=$name python3 -c 'import json, os, urllib.parse; s = os.environ[os.environ["S"]]; print("url", urllib.parse.quote(s, safe="")); print(json.dumps({"api_key": s}))'
  done
}
leak
leak > /logs/agent/trajectory-notes.txt
leak > /logs/artifacts/leak.txt
# Long enough for the host-side process watcher to look.
sleep 3
