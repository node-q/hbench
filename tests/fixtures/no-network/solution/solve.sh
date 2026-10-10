#!/bin/bash
if timeout 5 bash -c "echo > /dev/tcp/1.1.1.1/80" 2>/dev/null; then echo online; else echo offline; fi > net.txt
