#!/bin/bash
# Fetch official Node.js test files via sparse checkout.
# Downloads the parallel suite, its common harness, official fixtures used by
# selected tests, and CLI documentation consumed by process flag conformance.
#
# Usage:
#   bash scripts/fetch-node-tests.sh          # default: v24.x branch
#   bash scripts/fetch-node-tests.sh v22.x    # specific branch

set -euo pipefail

BRANCH="${1:-v24.x}"
TARGET="tests/node-compat/node"
REPO="https://github.com/nodejs/node.git"

if [ -d "$TARGET/.git" ]; then
    echo "Updating existing Node.js test checkout (branch: $BRANCH)..."
    cd "$TARGET"
    git fetch --depth=1 origin "$BRANCH"
    git sparse-checkout set --skip-checks \
        test/parallel test/common test/fixtures doc/api/cli.md
    git checkout "origin/$BRANCH" -- \
        test/parallel test/common test/fixtures doc/api/cli.md
    echo "Updated."
else
    echo "Cloning Node.js tests (branch: $BRANCH, sparse checkout)..."
    git clone --depth=1 --sparse --filter=blob:none \
        --branch "$BRANCH" "$REPO" "$TARGET"
    cd "$TARGET"
    git sparse-checkout set --skip-checks \
        test/parallel test/common test/fixtures doc/api/cli.md
    echo "Done. Tests available at $TARGET/test/parallel/"
fi

# Show stats
PARALLEL_COUNT=$(find test/parallel -name '*.js' -type f 2>/dev/null | wc -l | tr -d ' ')
echo "  test/parallel: $PARALLEL_COUNT test files"
