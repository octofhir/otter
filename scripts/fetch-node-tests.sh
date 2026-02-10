#!/bin/bash
# Fetch official Node.js test files via sparse checkout.
# Only downloads test/parallel/ and test/common/ (~50MB vs 300MB+ full repo).
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
    git checkout "origin/$BRANCH" -- test/parallel test/common
    echo "Updated."
else
    echo "Cloning Node.js tests (branch: $BRANCH, sparse checkout)..."
    git clone --depth=1 --sparse --filter=blob:none \
        --branch "$BRANCH" "$REPO" "$TARGET"
    cd "$TARGET"
    git sparse-checkout set test/parallel test/common
    echo "Done. Tests available at $TARGET/test/parallel/"
fi

# Show stats
PARALLEL_COUNT=$(find test/parallel -name '*.js' -type f 2>/dev/null | wc -l | tr -d ' ')
echo "  test/parallel: $PARALLEL_COUNT test files"
