#!/bin/sh
set -eu

role=$(tr -d '\n' < .gen42-canary-role)
head=$(git rev-parse HEAD)
sleep 300 &
child_pid=$!
pgid=$(ps -o pgid= -p $$ | tr -d ' ')
printf 'GEN42_CANARY role=%s head=%s shell_pid=%s child_pid=%s pgid=%s\n' \
  "$role" "$head" "$$" "$child_pid" "$pgid"
wait "$child_pid"
