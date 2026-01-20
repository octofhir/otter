#!/bin/bash
# Convenience script for running Node.js compatibility tests locally
#
# Usage:
#   ./run.sh                    # Run tests (fetch if needed)
#   ./run.sh --fetch            # Force fetch Node.js tests
#   ./run.sh --module path      # Run specific module
#   ./run.sh --verbose          # Verbose output
#   ./run.sh --help             # Show help

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Parse arguments
FETCH_TESTS=false
EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
  case $1 in
    --fetch)
      FETCH_TESTS=true
      shift
      ;;
    --help|-h)
      echo "Node.js Compatibility Test Runner"
      echo ""
      echo "Usage:"
      echo "  ./run.sh                    # Run tests (fetch if needed)"
      echo "  ./run.sh --fetch            # Force fetch Node.js tests"
      echo "  ./run.sh --module <name>    # Run specific module"
      echo "  ./run.sh --verbose          # Verbose output"
      echo "  ./run.sh --json             # JSON output"
      echo "  ./run.sh --help             # Show this help"
      echo ""
      echo "Examples:"
      echo "  ./run.sh --module path --verbose"
      echo "  ./run.sh --filter 'test-buffer-'"
      echo ""
      exit 0
      ;;
    *)
      EXTRA_ARGS+=("$1")
      shift
      ;;
  esac
done

# Check if otter is available
if ! command -v otter &> /dev/null; then
    echo -e "${RED}Error: 'otter' command not found${NC}"
    echo "Make sure otter is built and in your PATH:"
    echo "  cargo build --release -p otterjs"
    echo "  export PATH=\"\$PWD/target/release:\$PATH\""
    exit 1
fi

# Fetch tests if requested or if not present
if [ "$FETCH_TESTS" = true ] || [ ! -d "node-src/test" ]; then
    echo -e "${YELLOW}Fetching Node.js tests...${NC}"
    ./fetch-tests.sh
    echo ""
fi

# Run tests
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Running Node.js compatibility tests${NC}"
echo -e "${GREEN}========================================${NC}"
echo ""

# Run with all permissions needed for tests
otter run run-node-tests.ts \
    --allow-read \
    --allow-write \
    --allow-net \
    --allow-env \
    --allow-run \
    "${EXTRA_ARGS[@]}"

EXIT_CODE=$?

echo ""
if [ $EXIT_CODE -eq 0 ]; then
    echo -e "${GREEN}All tests passed!${NC}"
else
    echo -e "${YELLOW}Some tests failed. Check reports/latest.json for details.${NC}"
fi

echo -e "${BLUE}Report saved: reports/latest.json${NC}"

exit $EXIT_CODE
