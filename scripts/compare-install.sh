#!/usr/bin/env bash
set -euo pipefail

OUT_ROOT="${OUT_ROOT:-/tmp/otter-bun-compare}"
PROJECT_DIR="${PROJECT_DIR:-$OUT_ROOT/express-only}"
EXPRESS_RANGE="${EXPRESS_RANGE:-^4.18.3}"
WARM_RUNS="${WARM_RUNS:-5}"
TRACE_MODE="${TRACE_MODE:-auto}" # auto|dtruss|fs_usage|none (auto runs both)
TRACE_SECONDS="${TRACE_SECONDS:-5}"
TRACE_INSTALL_RUNS="${TRACE_INSTALL_RUNS:-0}"
TRACE_WARMUP_SLEEP="${TRACE_WARMUP_SLEEP:-0.7}"

usage() {
  cat <<'USAGE'
Usage:
  scripts/compare-install.sh [--no-dtruss] [--trace=auto|dtruss|fs_usage|none]

Environment overrides:
  OUT_ROOT=/tmp/otter-bun-compare
  PROJECT_DIR=/tmp/otter-bun-compare/express-only
  EXPRESS_RANGE=^4.18.3
  WARM_RUNS=5
  TRACE_MODE=auto
  TRACE_SECONDS=5

Outputs:
  /tmp/otter-bun-compare/*.txt

Notes:
  --no-dtruss disables only dtruss (fs_usage still runs if selected).
  Use --trace=none to disable all tracing.
USAGE
}

if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
  echo "Do not run this script with sudo/root." >&2
  echo "Run as your user; it will prompt for sudo only for tracing." >&2
  exit 1
fi

DO_DTRUSS=1
for arg in "$@"; do
  case "$arg" in
    -h|--help)
      usage
      exit 0
      ;;
    --no-dtruss)
      DO_DTRUSS=0
      ;;
    --trace=*)
      TRACE_MODE="${arg#--trace=}"
      ;;
    *)
      echo "Unknown argument: $arg" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing command: $1" >&2
    exit 1
  fi
}

need_cmd otter
need_cmd bun
need_cmd /usr/bin/time

mkdir -p "$OUT_ROOT"
if ! rm -rf "$PROJECT_DIR" 2>/dev/null; then
  echo "Failed to remove $PROJECT_DIR (likely root-owned from a previous sudo/dtruss run)." >&2
  echo "Fix with ONE of these, then re-run:" >&2
  echo "  sudo rm -rf \"$PROJECT_DIR\"" >&2
  echo "  sudo chown -R \"$(id -un)\":\"$(id -gn)\" \"$PROJECT_DIR\" && rm -rf \"$PROJECT_DIR\"" >&2
  exit 1
fi
mkdir -p "$PROJECT_DIR"

cat >"$PROJECT_DIR/package.json" <<JSON
{
  "name": "express-only",
  "private": true,
  "dependencies": {
    "express": "$EXPRESS_RANGE"
  }
}
JSON

{
  echo "=== Environment ==="
  sw_vers 2>/dev/null || true
  uname -a
  echo "otter: $(otter --version || true)"
  echo "bun:   $(bun --version || true)"
  echo
  echo "=== Vars ==="
  echo "TRACE_MODE=$TRACE_MODE"
  echo "TRACE_SECONDS=$TRACE_SECONDS"
  echo "TRACE_INSTALL_RUNS=$TRACE_INSTALL_RUNS"
} | tee "$OUT_ROOT/env.txt"

run_time() {
  local label="$1"
  shift
  local outfile="$OUT_ROOT/${label}.txt"
  echo "=== $label ===" | tee "$outfile"
  ( /usr/bin/time -l "$@" ) 2>&1 | tee -a "$outfile"
  echo >>"$outfile"
}

layout_stats() {
  local label="$1"
  local nm_dir="$2"
  local outfile="$OUT_ROOT/${label}.txt"
  {
    echo "=== $label ==="
    echo "node_modules: $nm_dir"
    echo "dirs:    $(find "$nm_dir" -type d | wc -l | tr -d ' ')"
    echo "files:   $(find "$nm_dir" -type f | wc -l | tr -d ' ')"
    echo "symlinks:$(find "$nm_dir" -type l | wc -l | tr -d ' ')"
    echo
    echo "top-level:"
    ls -la "$nm_dir" | head -n 40 || true
    echo
    if [[ -f "$nm_dir/express/package.json" ]]; then
      echo "express version:"
      rg -n '"name"|"version"' "$nm_dir/express/package.json" | head -n 5 || true
    fi
  } | tee "$outfile"
}

run_warm_loop() {
  local label="$1"
  shift
  local outfile="$OUT_ROOT/${label}.txt"
  echo "=== $label (warm runs: $WARM_RUNS) ===" | tee "$outfile"
  for i in $(seq 1 "$WARM_RUNS"); do
    rm -rf "$PROJECT_DIR/node_modules"
    echo "--- run $i ---" | tee -a "$outfile"
    ( /usr/bin/time -l "$@" >/dev/null ) 2>&1 | grep -E "[0-9.]+ real" | tail -n 1 | tee -a "$outfile"
  done
  echo >>"$outfile"
}

cd "$PROJECT_DIR"

# Otter: cold then warm
rm -rf node_modules otter.lock otter.lockb
run_time "otter_time_cold" otter install
rm -rf node_modules
run_time "otter_time_warm" otter install
layout_stats "otter_layout" "$PROJECT_DIR/node_modules"
run_warm_loop "otter_warm_runs" otter install

# Bun: cold then warm
rm -rf node_modules bun.lock bun.lockb
run_time "bun_time_cold" bun install
rm -rf node_modules
run_time "bun_time_warm" bun install
layout_stats "bun_layout" "$PROJECT_DIR/node_modules"
run_warm_loop "bun_warm_runs" bun install

trace_fs_usage() {
  local label="$1"
  local match="$2"
  shift 2
  local raw="$OUT_ROOT/${label}_fs_usage_raw.txt"
  local filtered="$OUT_ROOT/${label}_fs_usage_filtered.txt"
  local summary="$OUT_ROOT/${label}_fs_usage_summary.txt"
  local cmd_log="$OUT_ROOT/${label}_fs_usage_cmd.txt"

  echo "=== fs_usage ($label) ===" | tee "$cmd_log"
  echo "match: $match" | tee -a "$cmd_log"
  echo "cmd: $*" | tee -a "$cmd_log"

  # For very fast commands (bun can be ~20ms), starting fs_usage and then running a single install
  # often misses the window. We run multiple installs while fs_usage is collecting.
  local runs="$TRACE_INSTALL_RUNS"
  if [[ "$runs" -le 0 ]]; then
    runs=$(( TRACE_SECONDS * 10 ))
    if [[ "$runs" -lt 25 ]]; then runs=25; fi
  fi

  # Capture only filesys+pathname activity. Process filtering by name is unreliable for very short-lived
  # processes on some setups, so we capture system-wide and then post-filter by the final "proc.tid"
  # column (works for processes without spaces like "otter" and "bun").
  sudo fs_usage -w -f filesys -f pathname -t "$TRACE_SECONDS" >"$raw" 2>&1 &
  local fs_pid=$!

  sleep "$TRACE_WARMUP_SLEEP"
  echo "trace runs: $runs" | tee -a "$cmd_log"
  for i in $(seq 1 "$runs"); do
    rm -rf "$PROJECT_DIR/node_modules"
    "$@" >>"$cmd_log" 2>&1 || true
  done
  wait "$fs_pid" >/dev/null 2>&1 || true

  awk -v m="$match" '
    /^[0-9][0-9]:/ {
      proc = $NF
      if (proc == m || proc ~ ("^" m "\\.[0-9]+$")) print
    }
  ' "$raw" >"$filtered" || true

  {
    echo "runs: $runs"
    echo "lines: $(wc -l <"$filtered" | tr -d ' ')"
    echo
    echo "=== totals (top 60) ==="
  } >"$summary"

  awk '
    /^[0-9][0-9]:/ {
      call = $2
      sub(/\\[[^]]*\\]$/, "", call)
      sub(/:.*/, "", call)
      if (call != "") counts[call]++
    }
    END {
      for (c in counts) printf "%8d %s\n", counts[c], c
    }
  ' "$filtered" | sort -nr | head -n 60 >>"$summary" || true

  {
    echo
    echo "=== per-run avg (top 60) ==="
  } >>"$summary"

  awk -v runs="$runs" '
    /^[0-9][0-9]:/ {
      call = $2
      sub(/\\[[^]]*\\]$/, "", call)
      sub(/:.*/, "", call)
      if (call != "") counts[call]++
    }
    END {
      for (c in counts) printf "%10.2f %s\n", (counts[c] / runs), c
    }
  ' "$filtered" | sort -nr | head -n 60 >>"$summary" || true
}

trace_dtruss() {
  local label="$1"
  shift
  local out="$OUT_ROOT/${label}_dtruss_warm.txt"
  # IMPORTANT: avoid running the install command under sudo (root-owned node_modules/lockfiles).
  # We run the command as the current user and attach dtruss to its PID.
  local cmd_log="$OUT_ROOT/${label}_dtruss_cmd.txt"
  "$@" >"$cmd_log" 2>&1 &
  local cmd_pid=$!
  sleep 0.02
  sudo dtruss -c -f -p "$cmd_pid" 2>&1 | tee "$out" || true
  wait "$cmd_pid" || true
}

if [[ "$TRACE_MODE" != "none" ]]; then
  echo "=== tracing ===" | tee "$OUT_ROOT/trace_status.txt"
  echo "mode: $TRACE_MODE" | tee -a "$OUT_ROOT/trace_status.txt"
  echo "Will prompt for sudo password if needed." | tee -a "$OUT_ROOT/trace_status.txt"

  sudo -v || {
    echo "sudo failed; skipping tracing." | tee -a "$OUT_ROOT/trace_status.txt"
    TRACE_MODE="none"
  }

  if [[ "$DO_DTRUSS" -eq 1 && ( "$TRACE_MODE" == "dtruss" || "$TRACE_MODE" == "auto" ) ]]; then
    if command -v dtruss >/dev/null 2>&1; then
      rm -rf "$PROJECT_DIR/node_modules"
      trace_dtruss otter otter install

      rm -rf "$PROJECT_DIR/node_modules"
      trace_dtruss bun bun install
    else
      echo "dtruss not found." | tee -a "$OUT_ROOT/trace_status.txt"
    fi
  fi

  if [[ "$TRACE_MODE" == "fs_usage" || "$TRACE_MODE" == "auto" ]]; then
    rm -rf "$PROJECT_DIR/node_modules"
    trace_fs_usage otter otter otter install

    rm -rf "$PROJECT_DIR/node_modules"
    trace_fs_usage bun bun bun install
  fi
fi

echo
echo "Done. Send me these files (or at least tail -n 120 of the dtruss ones):"
echo "  $OUT_ROOT/otter_time_cold.txt"
echo "  $OUT_ROOT/otter_time_warm.txt"
echo "  $OUT_ROOT/bun_time_cold.txt"
echo "  $OUT_ROOT/bun_time_warm.txt"
echo "  $OUT_ROOT/otter_dtruss_warm.txt (if present)"
echo "  $OUT_ROOT/bun_dtruss_warm.txt (if present)"
echo "  $OUT_ROOT/otter_fs_usage_summary.txt (if present)"
echo "  $OUT_ROOT/bun_fs_usage_summary.txt (if present)"
