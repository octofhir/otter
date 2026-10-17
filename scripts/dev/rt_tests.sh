#!/bin/bash
# Run selected otter-runtime integration tests, then delete their binaries
# (each is ~125 MB). Usage: scripts/dev/rt_tests.sh test_a test_b ...
args=()
for t in "$@"; do args+=(--test "$t"); done
cargo test -p otter-runtime "${args[@]}" 2>&1 | grep -E "^test result|FAILED|panicked|^error|^     Running" | head -60
for t in "$@"; do rm -f target/debug/deps/"$t"-*; done
