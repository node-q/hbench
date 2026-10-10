#!/bin/bash
if [ "$(cat /app/net.txt)" = "offline" ]; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
