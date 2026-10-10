#!/bin/bash
if grep -q "hello-from-sidecar" /app/page.txt; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
