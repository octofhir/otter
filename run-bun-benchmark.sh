#!/bin/bash
set -e

echo "=== Bun HTTP Server Benchmark ==="
echo ""

# Start Bun server
echo "Starting Bun server..."
PORT=3001 bun run benchmarks/http/server-bun.ts &
BUN_PID=$!
sleep 2

# Check if server is running
if ! lsof -i :3001 > /dev/null 2>&1; then
    echo "ERROR: Server failed to start"
    kill $BUN_PID 2>/dev/null || true
    exit 1
fi

# Quick curl test
echo ""
echo "Testing server..."
curl -s http://localhost:3001/ && echo ""

# Run k6 benchmark
echo ""
echo "Running k6 benchmark (10 VUs, 10s)..."
echo ""
k6 run --quiet benchmarks/http/quick-test.js

# Cleanup
echo ""
echo "Stopping server..."
kill $BUN_PID 2>/dev/null || true
wait $BUN_PID 2>/dev/null || true

echo ""
echo "Done!"
