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
test262 *args:
    cargo run -p otter-test262 --bin test262 -- run {{args}}

# Run Test262 tests with filter (e.g., "literals")
test262-filter filter:
    cargo run -p otter-test262 --bin test262 -- run --filter {{filter}} -vv

# Run Test262 for specific directory (e.g., "language/expressions")
test262-dir dir:
    cargo run -p otter-test262 --bin test262 -- run --subdir {{dir}} -vv

# List Test262 tests (with optional filter)
test262-list filter="":
    cargo run -p otter-test262 -- run --list-only {{ if filter != "" { "--filter " + filter } else { "" } }}

# Run Test262 tests and save results to JSON
test262-save *args:
    cargo run -p otter-test262 --bin test262 -- run --save {{args}}

# Compare two saved Test262 result files
test262-compare base new:
    cargo run -p otter-test262 --bin test262 -- compare --base {{base}} --current {{new}}

# Run full test262 in crash-safe batches, merge results, generate conformance doc
test262-full *args:
    bash scripts/test262-full-run.sh {{args}}

# Generate ES_CONFORMANCE.md from latest test262 results
test262-conformance:
    cargo run -p otter-test262 --bin gen-conformance

# Run Test262 with TOML config override
test262-config config *args:
    cargo run -p otter-test262 --bin test262 -- run --config {{config}} {{args}}

# === Node.js Compatibility Tests ===

# Fetch official Node.js test files (sparse checkout of nodejs/node)
node-compat-fetch *args:
    bash scripts/fetch-node-tests.sh {{args}}

# Run all Node.js compatibility tests (auto-fetches if needed)
node-compat *args:
    @if [ ! -d "tests/node-compat/node/test/parallel" ]; then just node-compat-fetch; fi
    cargo run -p otter-node-compat -- {{args}}

# Run tests for a specific module (e.g. `just node-compat-module assert -vv`)
node-compat-module module *args:
    cargo run -p otter-node-compat -- --module {{module}} {{args}}

# Run and save results to reports/latest.json
node-compat-save *args:
    cargo run -p otter-node-compat -- --save {{args}}

# Run a specific module and save results
node-compat-save-module module *args:
    cargo run -p otter-node-compat -- --module {{module}} --save {{args}}

# Compare two result files
node-compat-compare base current:
    cargo run -p otter-node-compat -- compare --base {{base}} --current {{current}}

# Check for regressions against baseline
node-compat-check:
    cargo run -p otter-node-compat -- compare --base tests/node-compat/reports/baseline.json --current tests/node-compat/reports/latest.json

# Update baseline after intentional changes
node-compat-baseline:
    cp tests/node-compat/reports/latest.json tests/node-compat/reports/baseline.json
    @echo "Baseline updated from latest results"

# Show available modules and current status
node-compat-status:
    cargo run -p otter-node-compat -- status

# List tests without running (e.g. `just node-compat-list --module buffer`)
node-compat-list *args:
    cargo run -p otter-node-compat -- --list-only {{args}}

# Run with JSON output (for CI)
node-compat-json *args:
    cargo run -p otter-node-compat -- --json {{args}}
