#!/bin/bash
# Overruns the 3 s timeout, in the foreground and in a detached session.
setsid nohup bash -c 'sleep 6; echo late > /app/daemon.txt' >/dev/null 2>&1 &
sleep 6; echo late > /app/late.txt
