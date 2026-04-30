set shell := ["/bin/sh", "-cu"]

# Format all code
fmt:
    cargo fmt --all

# Run clippy lints
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run all tests
test:
    cargo test --all --all-features

# Build all targets
build:
    cargo build --all-targets

# Build release
release:
    cargo build --release -p otterjs

# Run the release-mode Tier 1 JIT benchmark gate
jit-tier1-gate *benchmarks:
    bash benchmarks/jit/tier1_release_gate.sh {{benchmarks}}

# Run CLI with arguments
run *args:
    cargo run -p otterjs -- {{args}}

# Clean build artifacts
clean:
    cargo clean

# === Examples ===

# Run a JavaScript example
example-js file:
    cargo run -p otterjs -- run examples/{{file}}.js

# Run a TypeScript example
example-ts file:
    cargo run -p otterjs -- run examples/{{file}}.ts

# Run all JavaScript examples
examples-js:
    @echo "=== Running JavaScript examples ==="
    cargo run -p otterjs -- run examples/basic.js
    @echo ""
    cargo run -p otterjs -- run examples/event_loop.js
    @echo ""
    cargo run -p otterjs -- run examples/http_fetch.js

# Run all TypeScript examples
examples-ts:
    @echo "=== Running TypeScript examples ==="
    @echo "\n--- basic.ts ---"
    cargo run -p otterjs -- run examples/basic.ts
    @echo "\n--- generics.ts ---"
    cargo run -p otterjs -- run examples/generics.ts
    @echo "\n--- async.ts ---"
    cargo run -p otterjs -- run examples/async.ts
    @echo "\n--- classes.ts ---"
    cargo run -p otterjs -- run examples/classes.ts

# Run all examples
examples: examples-js examples-ts

# List available examples
list-examples:
    @echo "JavaScript examples:"
    @ls -1 examples/*.js 2>/dev/null || echo "  (none)"
    @echo "\nTypeScript examples:"
    @ls -1 examples/*.ts 2>/dev/null || echo "  (none)"

# === Type Checking ===

# Type check TypeScript files
check *files:
    cargo run -p otterjs -- check {{files}}

# Type check all TypeScript examples
check-examples:
    cargo run -p otterjs -- check examples/*.ts

# Type check with a tsconfig.json project
check-project project:
    cargo run -p otterjs -- check -p {{project}} .

# === Test262 Conformance Tests ===

# Run Test262 tests (all). Pass extra args: just test262 --filter foo -vv
# Uses otter-runtime (new VM). Writes JSONL log to test262_results/run.jsonl
test262 *args:
    cargo run --profile test262 -p otter-test262 --bin test262 -- --log test262_results/run.jsonl {{args}}

# Run Test262 tests with filter (e.g., "literals")
test262-filter filter:
    cargo run --profile test262 -p otter-test262 --bin test262 -- --filter {{filter}} -vv --log test262_results/run.jsonl

# Run Test262 for specific directory (e.g., "built-ins/Math")
test262-dir dir:
    cargo run --profile test262 -p otter-test262 --bin test262 -- --subdir {{dir}} -vv --log test262_results/run.jsonl

# Run full test262 in crash-safe batches, merge results, generate conformance doc
test262-full *args:
    bash scripts/test262-full-run.sh {{args}}

# Generate ES_CONFORMANCE.md from latest test262 results
test262-conformance:
    cargo run --profile test262 -p otter-test262 --bin gen-conformance

# === Test262 (new engine, crates-next/otter-test262) ===
#
# The canonical conformance surface for the foundation runtime.
# See docs/new-engine/tasks/100-test262-conformance.md.

# Walk vendor/test262 without executing any tests (slice 101).
test262-next-dry *args:
    cargo run -p otter-test262 -- run --dry-run {{args}}

# Same as test262-next-dry — short alias.
test262-dry *args:
    cargo run -p otter-test262 -- run --dry-run {{args}}

# Pretty-print a test's frontmatter (slice 102).
test262-next-parse path:
    cargo run -p otter-test262 -- parse {{path}}

# Emit a histogram of `features:` tokens across the corpus (slice 102).
test262-next-features:
    cargo run -p otter-test262 -- run --dry-run --collect-features

# Run the corpus end-to-end (slice 103+). Pass --filter / --shard / etc.
test262-next *args:
    cargo run --release -p otter-test262 -- run {{args}}

# Diff a freshly produced report against an earlier baseline (slice 104).
test262-next-diff previous:
    cargo run -p otter-test262 -- diff {{previous}}

# Shard helper: just test262-next-shard 3/8 -- runs --shard 3/8 (slice 104).
test262-next-shard shard *args:
    cargo run --release -p otter-test262 -- run --shard {{shard}} {{args}}

# Run under the safety wrapper (slice 105). Linux-only ulimit + heap cap.
test262-next-safe *args:
    bash scripts/test262-safe.sh {{args}}

# === Node Compatibility Tests ===

# Fetch the official Node.js test suite used by the node-compat runner.
node-compat-fetch:
    bash scripts/fetch-node-tests.sh

# Run Node.js compatibility tests. Pass extra args, for example:
#   just node-compat process --limit 25
node-compat *args:
    cargo run -p otter-node-compat -- {{args}}

# Run Node.js compatibility tests with a substring filter.
node-compat-filter filter *modules:
    cargo run -p otter-node-compat -- {{modules}} --filter {{filter}}
