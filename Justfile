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
