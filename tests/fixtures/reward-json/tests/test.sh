#!/bin/bash
if grep -q ok /app/out.txt; then echo "{\"acc\": 1, \"partial\": 0.5}" > /logs/verifier/reward.json; else echo "{\"acc\": 0, \"partial\": 0}" > /logs/verifier/reward.json; fi
