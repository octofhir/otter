#!/bin/bash
set -e

echo "Starting Otter server..."
timeout 15 ./target/release/otter run benchmarks/http/test-serve-debug.ts --allow-net &
PID=$!
sleep 3

echo "Checking port..."
lsof -i :3001 || echo "No process on port 3001"

echo "Testing server..."
curl -v http://127.0.0.1:3001/ 2>&1 | head -20

echo "Killing server..."
kill $PID 2>/dev/null || true
wait $PID 2>/dev/null || true
echo "Done"
