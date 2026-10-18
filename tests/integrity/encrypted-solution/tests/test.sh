#!/bin/bash
if [ -e /app/main.py ] && [ ! -e /app/.oracle/solution.enc ]; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
