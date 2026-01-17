#!/bin/bash

# SQL Benchmark Runner
# Compares Otter vs Bun PostgreSQL performance
#
# Requirements:
#   - PostgreSQL running on localhost:5450 (or set DATABASE_URL)
#   - Otter release build
#   - Bun installed

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

OTTER_BIN="${PROJECT_ROOT}/target/release/otter"
if [ ! -f "$OTTER_BIN" ]; then
    OTTER_BIN="${PROJECT_ROOT}/target/debug/otter"
fi

echo -e "${YELLOW}╔══════════════════════════════════════════╗${NC}"
echo -e "${YELLOW}║     SQL Benchmark: Otter vs Bun          ║${NC}"
echo -e "${YELLOW}╚══════════════════════════════════════════╝${NC}"
echo ""

# Check PostgreSQL
echo -e "${BLUE}Checking PostgreSQL connection...${NC}"
if ! pg_isready -h localhost -p 5450 -q 2>/dev/null; then
    echo -e "${RED}PostgreSQL not available on localhost:5450${NC}"
    echo "Set DATABASE_URL or start PostgreSQL"
    exit 1
fi
echo -e "${GREEN}✓ PostgreSQL ready${NC}"
echo ""

# Run Otter benchmark
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${GREEN}Running Otter SQL Benchmark...${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
if [ -f "$OTTER_BIN" ]; then
    "$OTTER_BIN" run "$SCRIPT_DIR/bench-otter.ts" --allow-net
else
    echo -e "${RED}Otter not found. Build with: cargo build --release -p otterjs${NC}"
fi
echo ""

# Run Bun benchmark
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${GREEN}Running Bun SQL Benchmark...${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
if command -v bun &> /dev/null; then
    bun run "$SCRIPT_DIR/bench-bun.ts"
else
    echo -e "${RED}Bun not found. Install from https://bun.sh${NC}"
fi
echo ""

echo -e "${YELLOW}╔══════════════════════════════════════════╗${NC}"
echo -e "${YELLOW}║            Benchmark Complete            ║${NC}"
echo -e "${YELLOW}╚══════════════════════════════════════════╝${NC}"
echo ""
echo -e "${GREEN}Key takeaway:${NC}"
echo -e "  Otter COPY FROM is ~100x faster than single INSERTs"
echo -e "  Bun does NOT support COPY FROM/TO yet"
