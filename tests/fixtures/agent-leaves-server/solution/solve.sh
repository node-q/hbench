#!/bin/bash
# Starts a server in the background and exits; the verifier must still reach it.
nohup perl -MIO::Socket::INET -e '$s = IO::Socket::INET->new(LocalPort => 8000, Listen => 5, ReuseAddr => 1) or die;
  while ($c = $s->accept) { print $c "pong\n"; close $c }' >/dev/null 2>&1 &
sleep 1
