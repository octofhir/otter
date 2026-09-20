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

# Type-check the workspace as Windows sees it.
#
# Needs `cargo install cargo-xwin`; the MSVC SDK downloads once and is cached.
# Catches what a unix-only build cannot: `cfg(windows)` and `cfg(not(unix))`
# code paths compile nowhere else.
check-windows target="x86_64-pc-windows-msvc":
    cargo xwin check --workspace --all-features --target {{target}}

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

# Regenerate the Schubfach POW10 multiplier table
# (`crates/otter-vm/src/number/pow10_table.rs`). Run after editing
# the generator at `crates/otter-vm-codegen/src/bin/gen_pow10.rs`.
gen-pow10:
    cargo run -p otter-vm-codegen --bin gen-pow10 --release

# Build release
release:
    cargo build --release -p otter-cli

# Run CLI with arguments
run *args:
    cargo run -p otter-cli -- {{args}}

# Clean build artifacts
clean:
    cargo clean

# === Engine work loop ===

# Cost attribution for the engine kernels: retired instructions plus the axis
# counters (dispatch, IC misses, native boundary, deopt, GC) that explain
# them. Take the top line into the next change. Optional kernel-name filter.
cost *args:
    bash scripts/cost.sh {{args}}

# The bar: otter against node and bun on the same kernel, startup excluded.
# Ratios above 1.00x mean otter is slower by that factor. Optional filter.
vs *args:
    bash scripts/vs.sh {{args}}

# Diff otter's bytecode for a kernel against V8 Ignition's listing for the
# same function. Usage: just bcdiff dense-array [functionName]
bcdiff kernel function="engineKernel":
    bash scripts/bcdiff.sh {{kernel}} {{function}}

# Release admission gate: the bytecode verifier, its adversarial mutation
# corpus, and the code-space/snapshot admission tests, all in a release build.
verifier-gate:
    cargo test --release -p otter-bytecode
    cargo test --release -p otter-vm --lib code_space
    cargo test --release -p otter-vm --test snapshot_boundary
    cargo test --release -p otter-jit --test artifact_agreement

# Focused incremental checks: calls (default), gc, math, native, properties,
# or all. Full gate/Test262 remain the closing evidence.
quick *args:
    bash scripts/gate.sh --quick {{args}}

# Closing gate: fmt, clippy, vm/jit/bytecode tests, difftest across tiers,
# kernel ledger. Deliberately excludes test262 — that is the closing gate,
# compared as failing sets.
gate *args:
    bash scripts/gate.sh {{args}}

# === Standard JavaScript benchmark suites ===

# Fast baseline: V8 v7 + a small Octane smoke selection.
bench:
    bash benchmarks/run-all.sh

# Classic V8 v7 suite. Pass selected workload names as extra args.
bench-v8 *args:
    bash benchmarks/run-v8-v7.sh {{args}}

# Google's Octane suite. Pass selected workload names as extra args.
bench-octane *args:
    bash benchmarks/run-octane.sh {{args}}

# BrowserBench ARES-6. Pass selected workload keys: air basic babylon ml.
bench-ares6 *args:
    bash benchmarks/run-ares6.sh {{args}}

# V8 Web Tooling Benchmark. Example: just bench-web-tooling --only babel
bench-web-tooling *args:
    bash benchmarks/run-web-tooling.sh {{args}}

# yt-dlp/ejs real-world parser/transform workload.
bench-ejs:
    bash benchmarks/run-ejs.sh

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

# Run Test262 tests (all). Pass extra args: just test262 --filter foo
# Uses otter-runtime (new VM). Writes JSON + Markdown reports under test262_results/.
#
# Optimized: an unoptimized engine is slow enough that the regular-expression
# suites outrun the per-test budget and deep enough on the stack to abort, so
# the whole `property-escapes` family reads as timeouts and crashes that say
# nothing about conformance. The run is also five times shorter.
test262 *args:
    cargo run --release -p otter-test262 --bin otter-test262 -- run --output test262_results/run.json {{args}}

# Run Test262 tests with filter (e.g., "literals")
test262-filter filter:
    cargo run --release -p otter-test262 --bin otter-test262 -- run --filter {{filter}} --output test262_results/run.json

# Run Test262 for specific directory (e.g., "built-ins/Math")
test262-dir dir:
    cargo run --release -p otter-test262 --bin otter-test262 -- run --filter {{dir}} --output test262_results/run.json

# Run full test262 in crash-safe batches, merge results, generate conformance doc
test262-full *args:
    bash scripts/test262-full-run.sh {{args}}

# Render the interactive conformance dashboard from the latest merged
# baseline and refresh the copy shipped inside the contributor book.
test262-site:
    cargo run --release -p otter-test262 --bin otter-test262 -- site test262_results/latest.json --output test262_results/site/index.html
    cp test262_results/latest.json docs/site/public/conformance/data.json

# Generate ES_CONFORMANCE.md from a test262 results JSON (default: latest run).
test262-conformance input="test262_results/run.json":
    cargo run -p otter-test262 -- conformance {{input}} --output ES_CONFORMANCE.md

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

# Run the Node.js conformance corpus. With no arguments this runs every
# official test in test/parallel and test/sequential; arguments narrow the run
# to modules or flags (for example `just node-compat fs --limit 25`).
# Refreshes NODE_CONFORMANCE.md and the documentation site dashboard, and
# rebuilds and tests target/release/otter by default.
node-compat *args:
    cargo run -p otter-node-compat -- {{args}}

# Run Node.js compatibility tests with a substring filter.
node-compat-filter filter *modules:
    cargo run -p otter-node-compat -- {{modules}} --filter {{filter}}
