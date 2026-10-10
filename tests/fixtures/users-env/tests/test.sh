#!/bin/bash
r=0; [ "$(cat /work/who.txt)" = "agent" ] && [ "$(cat /work/foo.txt)" = "bar" ] && [ "$(pwd)" = "/work" ] && [ "$(whoami)" = "root" ] && [ "$EXPECT" = "from-default" ] && r=1; echo $r > /logs/verifier/reward.txt
