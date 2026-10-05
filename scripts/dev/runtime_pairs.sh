#!/usr/bin/env bash
# Run otter-runtime integration tests two at a time; print one summary line per binary.
set -u
while [ $# -gt 0 ]; do
  a=$1; b=${2:-}; shift; [ -n "$b" ] && shift
  args="--test $a"; [ -n "$b" ] && args="$args --test $b"
  out=$(timeout 3000 cargo test -p otter-runtime $args --no-fail-fast 2>&1)
  echo "$out" | grep -E "Running tests/|^test result|^test .* FAILED|panicked at" | sed 's/^/  /'
done
