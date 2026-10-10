#!/bin/bash
# A bare TCP connect can succeed under upstream's egress proxy; only a real HTTP exchange proves reachability.
if timeout 8 bash -c 'exec 3<>/dev/tcp/example.com/80; printf "GET / HTTP/1.0\r\nHost: example.com\r\n\r\n" >&3; head -c 4 <&3' 2>/dev/null | grep -q HTTP; then echo online; else echo offline; fi > net.txt
