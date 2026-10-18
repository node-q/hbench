#!/bin/bash
# Reward 1 if the server the agent left running answers.
exec 3<>/dev/tcp/127.0.0.1/8000 && read -r line <&3 && [ "$line" = pong ] && echo 1 > /logs/verifier/reward.txt || echo 0 > /logs/verifier/reward.txt
