#!/bin/bash
grep -q ok /tmp/p.txt && echo 1 > /logs/verifier/reward.txt || echo 0 > /logs/verifier/reward.txt
