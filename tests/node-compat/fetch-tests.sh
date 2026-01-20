#!/bin/bash
# Fetches Node.js test suite using sparse checkout for efficiency
# Usage: ./fetch-tests.sh [--version <node_version>]

set -e

# Configuration
NODE_VERSION="${NODE_VERSION:-v24.x}"
NODE_REPO="https://github.com/nodejs/node.git"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="$SCRIPT_DIR/node-src"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Parse arguments
while [[ $# -gt 0 ]]; do
  case $1 in
    --version|-v)
      NODE_VERSION="$2"
      shift 2
      ;;
    --help|-h)
      echo "Usage: $0 [--version <node_version>]"
      echo ""
      echo "Options:"
      echo "  --version, -v  Node.js version/branch to fetch (default: v24.x)"
      echo "  --help, -h     Show this help message"
      echo ""
      echo "Environment variables:"
      echo "  NODE_VERSION   Alternative way to set the version"
      exit 0
      ;;
    *)
      echo "Unknown option: $1"
      exit 1
      ;;
  esac
done

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Node.js Test Suite Fetcher${NC}"
echo -e "${GREEN}========================================${NC}"
echo -e "${BLUE}Version:${NC} $NODE_VERSION"
echo -e "${BLUE}Target:${NC}  $TARGET_DIR"
echo ""

if [ -d "$TARGET_DIR/.git" ]; then
    echo -e "${YELLOW}Updating existing checkout...${NC}"
    cd "$TARGET_DIR"

    # Fetch updates
    git fetch origin --depth 1

    # Try to checkout the specified version
    if git show-ref --verify --quiet "refs/remotes/origin/$NODE_VERSION"; then
        git checkout "origin/$NODE_VERSION" -- test/parallel test/sequential test/common test/fixtures 2>/dev/null || true
    else
        echo -e "${YELLOW}Branch $NODE_VERSION not found, using current...${NC}"
    fi

    cd "$SCRIPT_DIR"
else
    echo -e "${YELLOW}Creating new sparse checkout...${NC}"

    # Remove any existing non-git directory
    if [ -d "$TARGET_DIR" ]; then
        rm -rf "$TARGET_DIR"
    fi

    # Clone with sparse checkout
    git clone --depth 1 --filter=blob:none --sparse "$NODE_REPO" "$TARGET_DIR"
    cd "$TARGET_DIR"

    # Configure sparse checkout
    git sparse-checkout init --cone
    git sparse-checkout set test/parallel test/sequential test/common test/fixtures

    # Try to checkout specific version
    if [ "$NODE_VERSION" != "main" ]; then
        git fetch origin "$NODE_VERSION" --depth 1 2>/dev/null || echo -e "${YELLOW}Using default branch${NC}"
        git checkout "origin/$NODE_VERSION" 2>/dev/null || git checkout FETCH_HEAD 2>/dev/null || true
    fi

    cd "$SCRIPT_DIR"
fi

# Count tests
echo ""
echo -e "${GREEN}Counting tests...${NC}"

PARALLEL_COUNT=0
SEQUENTIAL_COUNT=0

if [ -d "$TARGET_DIR/test/parallel" ]; then
    PARALLEL_COUNT=$(find "$TARGET_DIR/test/parallel" -maxdepth 1 -name "test-*.js" 2>/dev/null | wc -l | tr -d ' ')
fi

if [ -d "$TARGET_DIR/test/sequential" ]; then
    SEQUENTIAL_COUNT=$(find "$TARGET_DIR/test/sequential" -maxdepth 1 -name "test-*.js" 2>/dev/null | wc -l | tr -d ' ')
fi

TOTAL=$((PARALLEL_COUNT + SEQUENTIAL_COUNT))

echo ""
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Fetch Complete${NC}"
echo -e "${GREEN}========================================${NC}"
echo -e "${BLUE}Parallel tests:${NC}   $PARALLEL_COUNT"
echo -e "${BLUE}Sequential tests:${NC} $SEQUENTIAL_COUNT"
echo -e "${BLUE}Total tests:${NC}      $TOTAL"
echo ""

if [ "$TOTAL" -eq 0 ]; then
    echo -e "${RED}Warning: No tests found! Check the Node.js version.${NC}"
    exit 1
fi

echo -e "${GREEN}Done! Run tests with: otter run run-node-tests.ts${NC}"
