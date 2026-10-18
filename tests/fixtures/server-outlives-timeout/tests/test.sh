#!/bin/bash
# Reward 1 if the agent stopped at its deadline and the server it detached still answers.
sleep 8
[ ! -e /app/late.txt ] && exec 3<>/dev/tcp/127.0.0.1/8000 && read -r line <&3 && [ "$line" = pong ] && echo 1 > /logs/verifier/reward.txt || echo 0 > /logs/verifier/reward.txt
