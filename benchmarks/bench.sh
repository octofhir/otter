#!/bin/bash

# Otter Benchmark Runner
# Compares Otter, Node.js, and Bun performance
#
# Usage:
#   ./benchmarks/bench.sh [benchmark-file]
#   ./benchmarks/bench.sh all
#   ./benchmarks/bench.sh startup
#   ./benchmarks/bench.sh cpu
#   ./benchmarks/bench.sh memory
#   ./benchmarks/bench.sh sql      # PostgreSQL benchmark (requires running DB)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Check for available runtimes
OTTER_BIN="${PROJECT_ROOT}/target/release/otter"
if [ ! -f "$OTTER_BIN" ]; then
    OTTER_BIN="${PROJECT_ROOT}/target/debug/otter"
fi

has_otter() {
    [ -f "$OTTER_BIN" ] || command -v otter &> /dev/null
}

has_node() {
    command -v node &> /dev/null
}

has_bun() {
    command -v bun &> /dev/null
}

# Run a single benchmark
run_benchmark() {
    local file="$1"
    local runtime="$2"

    case "$runtime" in
        otter)
            if has_otter; then
                if [ -f "$OTTER_BIN" ]; then
                    "$OTTER_BIN" run "$file" --allow-read
                else
                    otter run "$file" --allow-read
                fi
            else
                echo -e "${RED}Otter not found${NC}"
                return 1
            fi
            ;;
        node)
            if has_node; then
                # Node.js 22+ supports --experimental-strip-types
                node --experimental-strip-types "$file" 2>/dev/null || \
                    node "$file"
            else
                echo -e "${RED}Node.js not found${NC}"
                return 1
            fi
            ;;
        bun)
            if has_bun; then
                bun run "$file"
            else
                echo -e "${RED}Bun not found${NC}"
                return 1
            fi
            ;;
    esac
}

# Time a benchmark
time_benchmark() {
    local file="$1"
    local runtime="$2"

    echo -e "${BLUE}[$runtime]${NC} Running $file..."

    # Use GNU time if available, otherwise use bash time
    if command -v gtime &> /dev/null; then
        # macOS with GNU time installed (brew install gnu-time)
        gtime -f "  Real: %e s, User: %U s, Sys: %S s, Max RSS: %M KB" \
            bash -c "source '$SCRIPT_DIR/bench.sh' && run_benchmark '$file' '$runtime'" 2>&1
    elif [[ "$(uname)" == "Linux" ]] && [ -f /usr/bin/time ]; then
        # Linux with GNU time
        /usr/bin/time -f "  Real: %e s, User: %U s, Sys: %S s, Max RSS: %M KB" \
            bash -c "source '$SCRIPT_DIR/bench.sh' && run_benchmark '$file' '$runtime'" 2>&1
    else
        # macOS or fallback - use bash TIMEFORMAT
        TIMEFORMAT=$'  Real: %R s, User: %U s, Sys: %S s'
        { time run_benchmark "$file" "$runtime"; } 2>&1
    fi
}

# Run comparison
compare_benchmark() {
    local file="$1"

    echo ""
    echo -e "${GREEN}======================================${NC}"
    echo -e "${GREEN}Benchmark: $(basename "$file")${NC}"
    echo -e "${GREEN}======================================${NC}"

    if has_otter; then
        echo ""
        time_benchmark "$file" "otter"
    fi

    if has_node; then
        echo ""
        time_benchmark "$file" "node"
    fi

    if has_bun; then
        echo ""
        time_benchmark "$file" "bun"
    fi
}

# Export function for subshells
export -f run_benchmark has_otter has_node has_bun
export OTTER_BIN

# Main
main() {
    local target="${1:-all}"

    echo -e "${YELLOW}Otter Benchmark Suite${NC}"
    echo "=================================="

    # Show available runtimes
    echo "Available runtimes:"
    has_otter && echo -e "  ${GREEN}✓${NC} Otter" || echo -e "  ${RED}✗${NC} Otter"
    has_node && echo -e "  ${GREEN}✓${NC} Node.js $(node --version)" || echo -e "  ${RED}✗${NC} Node.js"
    has_bun && echo -e "  ${GREEN}✓${NC} Bun $(bun --version)" || echo -e "  ${RED}✗${NC} Bun"

    case "$target" in
        all)
            for f in "$SCRIPT_DIR"/startup/*.ts "$SCRIPT_DIR"/cpu/*.ts "$SCRIPT_DIR"/memory/*.ts; do
                [ -f "$f" ] && compare_benchmark "$f"
            done
            ;;
        startup)
            for f in "$SCRIPT_DIR"/startup/*.ts; do
                [ -f "$f" ] && compare_benchmark "$f"
            done
            ;;
        cpu)
            for f in "$SCRIPT_DIR"/cpu/*.ts; do
                [ -f "$f" ] && compare_benchmark "$f"
            done
            ;;
        memory)
            for f in "$SCRIPT_DIR"/memory/*.ts; do
                [ -f "$f" ] && compare_benchmark "$f"
            done
            ;;
        sql)
            # SQL benchmark has its own runner (requires PostgreSQL)
            if [ -f "$SCRIPT_DIR/sql/bench-sql.sh" ]; then
                "$SCRIPT_DIR/sql/bench-sql.sh"
            else
                echo -e "${RED}SQL benchmark not found${NC}"
            fi
            ;;
        *)
            # Assume it's a specific file
            if [ -f "$target" ]; then
                compare_benchmark "$target"
            elif [ -f "$SCRIPT_DIR/$target" ]; then
                compare_benchmark "$SCRIPT_DIR/$target"
            else
                echo -e "${RED}File not found: $target${NC}"
                exit 1
            fi
            ;;
    esac

    echo ""
    echo -e "${GREEN}Benchmark complete!${NC}"
}

main "$@"
