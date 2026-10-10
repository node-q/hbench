#!/bin/bash
exec 3<>/dev/tcp/web/8080; printf "GET / HTTP/1.0\r\n\r\n" >&3; cat <&3 > page.txt
