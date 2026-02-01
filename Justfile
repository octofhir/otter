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

# Run Test262 with TOML config override
test262-config config *args:
    cargo run -p otter-test262 --bin test262 -- run --config {{config}} {{args}}

# === Node.js Compatibility Tests ===

# Run Node.js compatibility test suite (fetches tests if needed)
node-compat:
    @echo "=== Node.js Compatibility Tests ==="
    cd tests/node-compat && ./run.sh --fetch

# Run Node.js tests (quick, no fetch)
node-compat-quick:
    cd tests/node-compat && ./run.sh

# Run tests for a specific module
node-compat-module module:
    cd tests/node-compat && ./run.sh --module {{module}} --verbose

# Fetch/update Node.js test suite
node-compat-fetch:
    cd tests/node-compat && ./fetch-tests.sh

# Check for test regressions
node-compat-check:
    cd tests/node-compat && otter run check-regression.ts --allow-read --allow-write

# Update baseline after intentional changes
node-compat-baseline:
    cp tests/node-compat/reports/latest.json tests/node-compat/reports/baseline.json
    @echo "Baseline updated from latest results"

# Show Node.js compat summary (requires jq)
node-compat-status:
    @if [ -f tests/node-compat/reports/latest.json ]; then \
        echo "=== Node.js Compatibility Status ==="; \
        cat tests/node-compat/reports/latest.json | jq -r '"Pass Rate: \(.summary.passRate) (\(.summary.passed)/\(.summary.total))"'; \
    else \
        echo "No report found. Run 'just node-compat' first."; \
    fi
