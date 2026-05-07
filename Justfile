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

# Report process-risk markers without failing the build.
process-audit:
    @echo "=== Porting debt markers ==="
    @grep -R "TODO(port)\|PERF(port)\|PORT NOTE\|PORT STATUS" crates docs --line-number 2>/dev/null || true
    @echo "\n=== Reachable placeholders ==="
    @grep -R "todo!()\|unimplemented!()" crates --include '*.rs' --line-number 2>/dev/null || true
    @echo "\n=== Unsafe usage for manual review ==="
    @grep -R "unsafe" crates/otter-gc crates/otter-jit crates/otter-modules --include '*.rs' --line-number 2>/dev/null || true
    @echo "\n=== Runtime-visible UTF-8/string conversions ==="
    @grep -R "from_utf8.*unwrap\|from_utf8_lossy\|to_string()" crates/otter-vm crates/otter-runtime crates/otter-web crates/otter-modules --include '*.rs' --line-number 2>/dev/null || true
    @echo "\n=== Potentially nondeterministic maps in spec-visible crates ==="
    @grep -R "HashMap\|FxHashMap" crates/otter-vm crates/otter-runtime crates/otter-web crates/otter-modules --include '*.rs' --line-number 2>/dev/null || true

# Build all targets
build:
    cargo build --all-targets

# Build release
release:
    cargo build --release -p otter-cli

# Run CLI with arguments
run *args:
    cargo run -p otter-cli -- {{args}}

# Clean build artifacts
clean:
    cargo clean

# === Examples ===

# Run a JavaScript example
example-js file:
    cargo run -p otter-cli -- run examples/{{file}}.js

# Run a TypeScript example
example-ts file:
    cargo run -p otter-cli -- run examples/{{file}}.ts

# Run all JavaScript examples
examples-js:
    @echo "=== Running JavaScript examples ==="
    cargo run -p otter-cli -- run examples/basic.js
    @echo ""
    cargo run -p otter-cli -- run examples/event_loop.js
    @echo ""
    cargo run -p otter-cli -- run examples/http_fetch.js

# Run all TypeScript examples
examples-ts:
    @echo "=== Running TypeScript examples ==="
    @echo "\n--- basic.ts ---"
    cargo run -p otter-cli -- run examples/basic.ts
    @echo "\n--- generics.ts ---"
    cargo run -p otter-cli -- run examples/generics.ts
    @echo "\n--- async.ts ---"
    cargo run -p otter-cli -- run examples/async.ts
    @echo "\n--- classes.ts ---"
    cargo run -p otter-cli -- run examples/classes.ts

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
    cargo run -p otter-cli -- check {{files}}

# Type check all TypeScript examples
check-examples:
    cargo run -p otter-cli -- check examples/*.ts

# Type check with a tsconfig.json project
check-project project:
    cargo run -p otter-cli -- check -p {{project}} .

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

# === Test262 (active engine, crates/otter-test262) ===
#
# The canonical conformance surface for the foundation runtime.
# See docs/book/src/contributing/test-harness.md.

# Walk vendor/test262 without executing any tests (slice 101).
test262-next-dry *args:
    cargo run -p otter-test262 -- run --dry-run {{args}}

# Same as test262-next-dry — short alias.
test262-dry *args:
    cargo run -p otter-test262 -- run --dry-run {{args}}

# Pretty-print a test's frontmatter (slice 102).
test262-next-parse path:
    cargo run -p otter-test262 -- parse {{path}}

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
