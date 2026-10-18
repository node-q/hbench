#!/bin/bash
# Detaches a server, then overruns the 10 s timeout. The overrun is stopped; the server is not.
(setsid nohup perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalPort => 8000, Listen => 5, ReuseAddr => 1) or die;
  while ($c = $s->accept) { print $c "pong\n"; close $c }' >/dev/null 2>&1 &)
sleep 15; echo late > /app/late.txt
