#!/bin/bash
if [ "$(/app/latest)" = new ] && [ ! -e /app/old.txt ]; then echo 1; else echo 0; fi > /logs/verifier/reward.txt
