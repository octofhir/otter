#!/usr/bin/env bash
# Retired instructions and CPU per HTTP request of one server process under a
# fixed number of requests from oha.
# usage: scripts/dev/http_instr_per_request.sh <requests> <server command...>
set -u
N=${1:?requests}; shift
PORT=3901
/usr/bin/time -l "$@" >/dev/null 2>benchmarks/results/http/instr_time.txt &
tpid=$!
sleep 2
oha -n "$N" -c 50 --no-tui "http://localhost:$PORT/" >/dev/null 2>&1
kill -TERM "$(pgrep -P $tpid)"
wait $tpid 2>/dev/null
instr=$(awk '/instructions retired/ {print $1}' benchmarks/results/http/instr_time.txt)
user=$(awk '/user/ {for (i=1;i<=NF;i++) if ($i=="user") print $(i-1)}' benchmarks/results/http/instr_time.txt | head -1)
echo "instructions/request $((instr / N))  user-seconds $user"
