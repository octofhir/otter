#!/usr/bin/env bash
# JSON benchmark baseline — compares otter vs node vs bun vs deno.
#
# Usage:
#   ./benchmarks/cpu/json_baseline.sh
#   SKIP_BUILD=1 ./benchmarks/cpu/json_baseline.sh   # skip cargo build
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
BENCH_FILE="$SCRIPT_DIR/json_bench.js"
OTTER_BIN="${OTTER_BIN:-$REPO_ROOT/target/release/otterjs}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

header() { printf "\n${BOLD}${CYAN}═══ %s ═══${NC}\n" "$1"; }

# Build otter in release mode
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  printf "${YELLOW}Building otter (release)...${NC}\n"
  cargo build --release -p otterjs 2>/dev/null
fi

# Check available runtimes
runtimes=()
if command -v node &>/dev/null; then runtimes+=(node); fi
if command -v bun &>/dev/null; then runtimes+=(bun); fi
if command -v deno &>/dev/null; then runtimes+=(deno); fi
if [[ -x "$OTTER_BIN" ]]; then runtimes+=(otter); fi

if [[ ${#runtimes[@]} -lt 2 ]]; then
  echo "Need at least 2 runtimes. Found: ${runtimes[*]}"
  exit 1
fi

printf "${BOLD}Runtimes:${NC}"
for rt in "${runtimes[@]}"; do
  case "$rt" in
    node)  ver="$(node -v 2>/dev/null)" ;;
    bun)   ver="$(bun --version 2>/dev/null)" ;;
    deno)  ver="$(deno --version 2>/dev/null | head -1)" ;;
    otter) ver="$("$OTTER_BIN" --version 2>/dev/null || echo "dev")" ;;
  esac
  printf "  %s (%s)" "$rt" "$ver"
done
echo ""

run_bench() {
  local name="$1"
  local cmd="$2"
  header "$name"
  if timeout 120 bash -c "$cmd" 2>&1; then
    return 0
  else
    printf "${RED}  FAILED or TIMEOUT${NC}\n"
    return 1
  fi
}

for rt in "${runtimes[@]}"; do
  case "$rt" in
    node)  run_bench "Node.js" "node $BENCH_FILE" || true ;;
    bun)   run_bench "Bun" "bun $BENCH_FILE" || true ;;
    deno)  run_bench "Deno" "deno run --allow-all $BENCH_FILE" || true ;;
    otter) run_bench "Otter" "$OTTER_BIN run $BENCH_FILE" || true ;;
  esac
done

printf "\n${BOLD}${GREEN}Done.${NC}\n"
