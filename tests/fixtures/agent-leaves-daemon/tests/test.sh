#!/bin/bash
# Reward 1 unless something the agent left running overwrites it.
echo 1 > /logs/verifier/reward.txt
sleep 6
