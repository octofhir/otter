#!/bin/bash
set -e

# Default settings
BATCH_SIZE=20
TIMEOUT=20
TEST_DIR="tests/test262/test"
TARGET_DIR="target/debug"

# Helper function
usage() {
    echo "Usage: $0 [options] <pattern>"
    echo "Options:"
    echo "  --batch-size <n>   Number of tests per batch (default: $BATCH_SIZE)"
    echo "  --timeout <n>      Timeout in seconds per test (default: $TIMEOUT)"
    echo "  --release          Use release build (default: debug)"
    echo ""
    echo "Example:"
    echo "  $0 --batch-size 10 language/statements/for-of"
    exit 1
}

# Parse args
POSITIONAL_ARGS=()
while [[ $# -gt 0 ]]; do
  case $1 in
    --batch-size)
      BATCH_SIZE="$2"
      shift 2
      ;;
    --timeout)
      TIMEOUT="$2"
      shift 2
      ;;
    --release)
      TARGET_DIR="target/release"
      shift 1
      ;;
    -*|--*)
      echo "Unknown option $1"
      usage
      ;;
    *)
      POSITIONAL_ARGS+=("$1")
      shift # past argument
      ;;
  esac
done

set -- "${POSITIONAL_ARGS[@]}"

PATTERN="$1"
if [ -z "$PATTERN" ]; then
    usage
fi

EXECUTABLE="$TARGET_DIR/test262"

if [ ! -f "$EXECUTABLE" ]; then
    echo "Error: Executable not found at $EXECUTABLE"
    echo "Please build it first: cargo build -p otter-test262 (--release)"
    exit 1
fi

echo "Finding tests matching '$PATTERN'..."
# Find files matching the pattern within the test directory
# We assume the pattern is a subdirectory or part of the path
FOUND_FILES=$(find "$TEST_DIR" -path "*$PATTERN*.js" -not -path "*_FIXTURE.js")

NUM_FILES=$(echo "$FOUND_FILES" | wc -w)
echo "Found $NUM_FILES tests."

if [ "$NUM_FILES" -eq 0 ]; then
    echo "No tests found."
    exit 0
fi

echo "Running in batches of $BATCH_SIZE with timeout ${TIMEOUT}s..."

# Split files into batches and run
echo "$FOUND_FILES" | xargs -n "$BATCH_SIZE" sh -c '
    echo "=== Batch Start ==="
    '"$EXECUTABLE"' --timeout '"$TIMEOUT"' "$@"
    RET=$?
    echo "=== Batch End (Exit Code: $RET) ==="
    # We dont exit on failure, we want to continue running other batches
    # But if the tool crashes hard (segfault), xargs might stop?
    # xargs continues by default unless -e is used or strict errors
' --

echo "Done."
