#!/bin/bash
# Both planted files must be gone before grading; the real verdict is reward.txt.
if [ -e /logs/verifier/reward.json ] || [ -e /tests/planted ]; then echo 0; else echo 1; fi > /logs/verifier/reward.txt
