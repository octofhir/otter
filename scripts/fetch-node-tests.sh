#!/bin/bash
# Fetch official Node.js test files via sparse checkout.
# Downloads the parallel and sequential suites, their common harness, the
# official fixtures they load, and the CLI documentation consumed by process
# flag conformance.
#
# Tracks nodejs/node `main` so conformance is measured against the same moving
# target Node itself develops against.
#
# Usage:
#   bash scripts/fetch-node-tests.sh          # default: main branch
#   bash scripts/fetch-node-tests.sh v24.x    # specific branch

set -euo pipefail

BRANCH="${1:-main}"
TARGET="tests/node-compat/node"
REPO="https://github.com/nodejs/node.git"
PATHS=(test/parallel test/sequential test/common test/fixtures doc/api/cli.md)

if [ -d "$TARGET/.git" ]; then
    echo "Updating existing Node.js test checkout (branch: $BRANCH)..."
    cd "$TARGET"
    # The clone is shallow and single-branch, so a previously fetched branch is
    # the only remote-tracking ref that exists; check out the fetched tip
    # directly instead of assuming `origin/$BRANCH` resolves.
    git fetch --depth=1 origin "$BRANCH"
    git sparse-checkout set --skip-checks "${PATHS[@]}"
    git checkout --detach --force FETCH_HEAD
    echo "Updated."
else
    echo "Cloning Node.js tests (branch: $BRANCH, sparse checkout)..."
    git clone --depth=1 --sparse --filter=blob:none \
        --branch "$BRANCH" "$REPO" "$TARGET"
    cd "$TARGET"
    git sparse-checkout set --skip-checks "${PATHS[@]}"
    echo "Done. Tests available at $TARGET/test/"
fi

echo "  checkout: $(git rev-parse HEAD)"
for suite in parallel sequential; do
    count=$(find "test/$suite" -type f \( -name '*.js' -o -name '*.mjs' \) 2>/dev/null | wc -l | tr -d ' ')
    echo "  test/$suite: $count test files"
done
