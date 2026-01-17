#!/bin/bash

# HTTP Server Benchmark Script
#
# Compares Bun.serve() vs Otter.serve() performance using k6.
#
# Usage:
#   ./benchmarks/http/bench-http.sh          # Run both
#   ./benchmarks/http/bench-http.sh bun      # Run Bun only
#   ./benchmarks/http/bench-http.sh otter    # Run Otter only
#   ./benchmarks/http/bench-http.sh quick    # Quick test (10s)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

# Ports
BUN_PORT=3000
OTTER_PORT=3001

# Check dependencies
check_deps() {
    if ! command -v k6 &> /dev/null; then
        echo -e "${RED}Error: k6 is not installed${NC}"
        echo "Install with: brew install k6"
        exit 1
    fi

    if ! command -v bun &> /dev/null; then
        echo -e "${YELLOW}Warning: bun is not installed${NC}"
    fi

    OTTER_BIN="${PROJECT_ROOT}/target/release/otter"
    if [ ! -f "$OTTER_BIN" ]; then
        OTTER_BIN="${PROJECT_ROOT}/target/debug/otter"
    fi

    if [ ! -f "$OTTER_BIN" ]; then
        echo -e "${YELLOW}Warning: otter binary not found. Build with: cargo build --release${NC}"
    fi
}

# Start Bun server
start_bun() {
    echo -e "${BLUE}Starting Bun server on port $BUN_PORT...${NC}"
    PORT=$BUN_PORT bun run "$SCRIPT_DIR/server-bun.ts" &
    BUN_PID=$!
    sleep 2

    if ! kill -0 $BUN_PID 2>/dev/null; then
        echo -e "${RED}Failed to start Bun server${NC}"
        return 1
    fi

    echo -e "${GREEN}Bun server started (PID: $BUN_PID)${NC}"
}

# Start Otter server
start_otter() {
    echo -e "${BLUE}Starting Otter server on port $OTTER_PORT...${NC}"

    if [ ! -f "$OTTER_BIN" ]; then
        echo -e "${RED}Otter binary not found${NC}"
        return 1
    fi

    PORT=$OTTER_PORT "$OTTER_BIN" run "$SCRIPT_DIR/server-otter.ts" --allow-net --timeout 0 &
    OTTER_PID=$!
    sleep 2

    if ! kill -0 $OTTER_PID 2>/dev/null; then
        echo -e "${RED}Failed to start Otter server${NC}"
        return 1
    fi

    echo -e "${GREEN}Otter server started (PID: $OTTER_PID)${NC}"
}

# Stop servers
cleanup() {
    echo -e "\n${YELLOW}Cleaning up...${NC}"
    [ -n "$BUN_PID" ] && kill $BUN_PID 2>/dev/null || true
    [ -n "$OTTER_PID" ] && kill $OTTER_PID 2>/dev/null || true
    wait 2>/dev/null || true
}

trap cleanup EXIT

# Run k6 benchmark
run_benchmark() {
    local name=$1
    local url=$2
    local duration=${3:-"30s"}

    echo -e "\n${CYAN}========================================${NC}"
    echo -e "${CYAN}Benchmarking: $name${NC}"
    echo -e "${CYAN}URL: $url${NC}"
    echo -e "${CYAN}========================================${NC}\n"

    # Quick test or full test
    if [ "$duration" == "quick" ]; then
        k6 run "$SCRIPT_DIR/load-test.js" \
            --env URL="$url" \
            --env NAME="$name" \
            --duration 10s \
            --vus 10 \
            --no-thresholds
    else
        k6 run "$SCRIPT_DIR/load-test.js" \
            --env URL="$url" \
            --env NAME="$name"
    fi
}

# Simple quick benchmark without k6 scenarios
run_quick_benchmark() {
    local name=$1
    local url=$2

    echo -e "\n${CYAN}========================================${NC}"
    echo -e "${CYAN}Quick Benchmark: $name${NC}"
    echo -e "${CYAN}URL: $url${NC}"
    echo -e "${CYAN}========================================${NC}\n"

    k6 run - --env URL="$url" --env NAME="$name" <<'EOF'
import http from 'k6/http';
import { check } from 'k6';

export const options = {
    vus: 50,
    duration: '10s',
};

export default function () {
    const res = http.get(`${__ENV.URL}/`);
    check(res, { 'status 200': (r) => r.status === 200 });
}
EOF
}

# Main
main() {
    local target="${1:-all}"

    echo -e "${YELLOW}======================================${NC}"
    echo -e "${YELLOW}HTTP Server Benchmark${NC}"
    echo -e "${YELLOW}======================================${NC}"

    check_deps

    case "$target" in
        bun)
            start_bun
            run_benchmark "bun" "http://localhost:$BUN_PORT"
            ;;
        otter)
            start_otter
            run_benchmark "otter" "http://localhost:$OTTER_PORT"
            ;;
        quick)
            if command -v bun &> /dev/null; then
                start_bun
            fi
            if [ -f "$OTTER_BIN" ]; then
                start_otter
            fi
            sleep 1

            if [ -n "$BUN_PID" ]; then
                run_quick_benchmark "bun" "http://localhost:$BUN_PORT"
            fi
            if [ -n "$OTTER_PID" ]; then
                run_quick_benchmark "otter" "http://localhost:$OTTER_PORT"
            fi
            ;;
        all|*)
            # Start both servers
            if command -v bun &> /dev/null; then
                start_bun
            fi
            if [ -f "$OTTER_BIN" ]; then
                start_otter
            fi

            sleep 1

            # Run benchmarks sequentially
            if [ -n "$BUN_PID" ]; then
                run_benchmark "bun" "http://localhost:$BUN_PORT"
            fi

            echo -e "\n${YELLOW}Waiting 5s before next benchmark...${NC}\n"
            sleep 5

            if [ -n "$OTTER_PID" ]; then
                run_benchmark "otter" "http://localhost:$OTTER_PORT"
            fi
            ;;
    esac

    echo -e "\n${GREEN}======================================${NC}"
    echo -e "${GREEN}Benchmark Complete!${NC}"
    echo -e "${GREEN}======================================${NC}"
}

main "$@"
