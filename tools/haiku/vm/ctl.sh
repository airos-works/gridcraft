#!/usr/bin/env bash
# Sends control-channel requests (JSON objects without "id", one per argument) to GridCraft
# running in the test VM with --control 7979, and prints the replies. See
# docs/control-protocol.md for the methods. The first line authenticates with the run's token:
# GRIDCRAFT_CONTROL_TOKEN when the app was started with that token, else the one it wrote to
# ~/.cache/gridcraft/control-7979.token.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ -n ${GRIDCRAFT_CONTROL_TOKEN:-} ]]; then
    token=$GRIDCRAFT_CONTROL_TOKEN
else
    token=$(bash "$HERE/haiku.sh" 'cat ~/.cache/gridcraft/control-7979.token')
fi
reqs="{\"id\":0,\"method\":\"auth\",\"params\":{\"token\":\"$token\"}}"$'\n'
n=1
for r in "$@"; do
    reqs+="${r%\}},\"id\":$n}"$'\n'
    n=$((n + 1))
done
printf '%s' "$reqs" | bash "$HERE/haiku.sh" 'nc -q 3 127.0.0.1 7979 2>/dev/null || nc 127.0.0.1 7979'
