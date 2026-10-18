#!/usr/bin/env python3
"""List the pins in src/agents/installed/pins.rs that have a newer release. Read-only: bump by hand."""
import json
import pathlib
import re
import subprocess
import sys
import urllib.request

PINS = pathlib.Path(__file__).resolve().parents[2] / "src/agents/installed/pins.rs"
PIN = re.compile(r'pub const (\w+): Pin = Pin::(npm|pypi|github|commit|url)\("([^"]+)", "([^"]+)"(?:, "([^"]+)")?\);')


def fetch(url):
    req = urllib.request.Request(url, headers={"User-Agent": "hbench-check-pins"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return r.read().decode()


def gh(path):
    return json.loads(subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout)


def latest(kind, name, url):
    if kind == "npm":
        return json.loads(fetch(f"https://registry.npmjs.org/{name}"))["dist-tags"]["latest"]
    if kind == "pypi":
        return json.loads(fetch(f"https://pypi.org/pypi/{name}/json"))["info"]["version"]
    if kind == "github":
        return gh(f"repos/{name}/releases/latest")["tag_name"].removeprefix("v")
    if kind == "commit":
        return gh(f"repos/{name}/commits/HEAD")["sha"]
    text = fetch(url).strip()
    return (json.loads(text)["version"] if text.startswith("{") else text).removeprefix("v")


stale = 0
for const, kind, name, version, url in PIN.findall(PINS.read_text()):
    try:
        newest = latest(kind, name, url)
    except Exception as e:  # noqa: BLE001 - report and keep going
        print(f"?  {const:18} {name}: {e}")
        continue
    if newest != version.removeprefix("v"):
        stale += 1
        print(f"-> {const:18} {name} {version} -> {newest}")
print(f"{stale} pin(s) behind")
sys.exit(0)
