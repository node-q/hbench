#!/bin/bash
# Reward 1 when the agent's commands started in /custom-workdir: workdir.txt must be
# there and hold that path.

if [ -f /custom-workdir/workdir.txt ] && [ "$(cat /custom-workdir/workdir.txt)" = /custom-workdir ]; then
  echo 1 > /logs/verifier/reward.txt
else
  echo "expected /custom-workdir/workdir.txt to contain /custom-workdir" >&2
  echo 0 > /logs/verifier/reward.txt
fi
