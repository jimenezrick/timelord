#!/bin/bash

set -eu

echo "Hi there, IPv4: $(date)" | socat STDIO UDP4-DATAGRAM:224.0.0.1:9090,ip-multicast-if=127.0.0.1
