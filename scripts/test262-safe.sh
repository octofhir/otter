#!/usr/bin/env bash
# Safety wrapper around `otter-test262 run`.
#
# Three layers per docs/new-engine/tasks/100-test262-conformance.md
# §"Safety controls":
#   1. In-engine cooperative cancellation (`--timeout`).
#   2. Per-test heap cap (`--max-heap-bytes`).
#   3. OS virtual-memory cap (`ulimit -v`, Linux).
#
# Tunables:
#   MAX_HEAP_BYTES     per-test heap cap (default 512 MB)
#   ULIMIT_VIRTUAL_KB  process VM cap in KB (default 4 GB)
#   TIMEOUT_MS         per-test timeout in ms (default 5000)
#
# Usage:
#   bash scripts/test262-safe.sh                     # full sweep
#   bash scripts/test262-safe.sh --shard 3/8         # one shard
#   bash scripts/test262-safe.sh --filter built-ins/Math
#   bash scripts/test262-safe.sh built-ins/Array     # shorthand for --filter
#   bash scripts/test262-safe.sh --allow-debug ...   # debug build OK
#
# The runner refuses to launch on debug builds without --allow-debug.
# vendor/test262 is auto-initialised if missing; the wrapper aborts
# if a re-execution still finds the submodule empty.

set -uo pipefail

ALLOW_DEBUG=0
PROFILE_FLAG="--release"
PASSTHROUGH=()
FILTER_SEEN=0
EXPECT_VALUE_FOR=""

normalize_filter() {
    case "$1" in
        built-ins/Array)
            printf '%s\n' "built-ins/Array/"
            ;;
        *)
            printf '%s\n' "$1"
            ;;
    esac
}

for arg in "$@"; do
    if [[ -n "$EXPECT_VALUE_FOR" ]]; then
        if [[ "$EXPECT_VALUE_FOR" == "--filter" ]]; then
            PASSTHROUGH+=("$(normalize_filter "$arg")")
        else
            PASSTHROUGH+=("$arg")
        fi
        EXPECT_VALUE_FOR=""
        continue
    fi
    case "$arg" in
        --allow-debug)
            ALLOW_DEBUG=1
            PROFILE_FLAG=""
            ;;
        --filter)
            FILTER_SEEN=1
            PASSTHROUGH+=("$arg")
            EXPECT_VALUE_FOR="$arg"
            ;;
        --filter=*)
            FILTER_SEEN=1
            PASSTHROUGH+=("--filter=$(normalize_filter "${arg#--filter=}")")
            ;;
        --shard|--timeout|--max-heap-bytes|--output|--config|--cursor|--resume)
            PASSTHROUGH+=("$arg")
            EXPECT_VALUE_FOR="$arg"
            ;;
        --*)
            PASSTHROUGH+=("$arg")
            ;;
        *)
            if [[ "$FILTER_SEEN" -eq 0 ]]; then
                FILTER_SEEN=1
                PASSTHROUGH+=("--filter" "$(normalize_filter "$arg")")
            else
                PASSTHROUGH+=("$arg")
            fi
            ;;
    esac
done

MAX_HEAP_BYTES="${MAX_HEAP_BYTES:-536870912}"   # 512 MB
ULIMIT_VIRTUAL_KB="${ULIMIT_VIRTUAL_KB:-4194304}" # 4 GB
TIMEOUT_MS="${TIMEOUT_MS:-5000}"

repo_root() {
    git rev-parse --show-toplevel 2>/dev/null || pwd
}
ROOT="$(repo_root)"
cd "$ROOT"

ensure_submodule() {
    local sub="vendor/test262"
    if [[ ! -d "$sub" || ! -d "$sub/test" || -z "$(ls -A "$sub/test" 2>/dev/null)" ]]; then
        echo "vendor/test262 missing — running: git submodule update --init --recursive vendor/test262" >&2
        git submodule update --init --recursive vendor/test262 || return 1
    fi
    if [[ ! -d "$sub/test" || -z "$(ls -A "$sub/test" 2>/dev/null)" ]]; then
        echo "error: vendor/test262 still empty after submodule update — aborting" >&2
        return 1
    fi
    return 0
}
ensure_submodule || exit 2

if [[ "$ALLOW_DEBUG" -eq 0 ]]; then
    echo "test262-safe: --release build (use --allow-debug to override)"
fi

echo "test262-safe: heap=${MAX_HEAP_BYTES} bytes ($((MAX_HEAP_BYTES / 1024 / 1024)) MB)"
echo "test262-safe: timeout=${TIMEOUT_MS} ms"
if [[ "$(uname)" == "Linux" ]]; then
    echo "test262-safe: ulimit -v ${ULIMIT_VIRTUAL_KB} KB ($((ULIMIT_VIRTUAL_KB / 1024)) MB)"
fi

# `ulimit -v` is Linux-only; macOS rejects it silently.
(
    if [[ "$(uname)" == "Linux" ]]; then
        ulimit -v "$ULIMIT_VIRTUAL_KB" 2>/dev/null || true
    fi
    set -- ${PROFILE_FLAG} -p otter-test262 -- run \
        --timeout "${TIMEOUT_MS}" \
        --max-heap-bytes "${MAX_HEAP_BYTES}" \
        "${PASSTHROUGH[@]}"
    exec cargo run "$@"
)
EXIT=$?

# Watchdog hard-kill exit codes (137 = SIGKILL, 139 = SIGSEGV,
# 86 = our own internal hard-kill marker). Re-exec once to give the
# runner a clean restart on those.
if [[ "$EXIT" -eq 86 || "$EXIT" -eq 137 || "$EXIT" -eq 139 ]]; then
    echo "test262-safe: hard-kill exit $EXIT — re-executing once" >&2
    if [[ -n "${OTTER_TEST262_SAFE_REENTERED:-}" ]]; then
        echo "test262-safe: already re-executed once — giving up" >&2
        exit "$EXIT"
    fi
    OTTER_TEST262_SAFE_REENTERED=1 exec bash "$0" "$@"
fi

exit "$EXIT"
