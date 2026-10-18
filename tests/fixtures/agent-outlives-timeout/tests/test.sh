#!/bin/bash
# Reward 1 only if nothing the agent started was still running to write these.
sleep 8
if [ -e /app/late.txt ] || [ -e /app/daemon.txt ]; then echo 0; else echo 1; fi > /logs/verifier/reward.txt
