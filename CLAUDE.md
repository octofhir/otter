# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

See [AGENTS.md](./AGENTS.md) for detailed coding agent instructions.

## Quick Start

```bash
# Build
cargo build                              # Debug build
cargo build --release -p otterjs         # Release CLI binary

# Test
cargo test --all --all-features          # All tests
cargo test -p otter-vm                   # Target VM tests
cargo test -p otter-runtime              # Target runtime tests

# Lint and format
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings

# Run scripts
cargo run -p otterjs -- run examples/basic.ts
cargo run -p otterjs -- examples/basic.ts    # Shorthand

# Using justfile (recommended)
just fmt && just lint && just test
just run examples/basic.ts
just test262-filter "Array/prototype/map"
```

Current fast-path CLI surface during migration:
- enabled: `run`, direct file execution, `-e`, `-p`, package-management commands
- disabled: `repl`, `test`, `build`

## Key Architecture

Target stack:
- `crates/otter-gc`
- `crates/otter-vm`
- `crates/otter-runtime`
- `crates/otter-jit`

Legacy stack being retired:
- `crates/otter-engine`
- `crates/otter-vm-runtime`
- `crates/otter-vm-core`

Migration rules:
- New runtime/VM/API work must target the target stack.
- Do not add new dependencies from target-stack crates to legacy crates.
- Legacy crates may remain in the repo temporarily, but once no active workspace member depends on them they should be removed from `[workspace].members` so they stop participating in compilation.
- Removing a crate from the workspace is not enough if a live crate still depends on it by path.

### Crate Layering (bottom-up)

```
otterjs (CLI)
    ↓
target host/runtime integration layer
    ↓
otter-runtime
    ↓
otter-vm
    ↓
otter-gc
```

### Critical Implementation Details

1. **Value Representation**: The target value model lives in `crates/otter-vm/src/value.rs`.

2. **Object Model**: The target object model lives in `crates/otter-vm/src/object.rs`.

3. **GC Safety**: Use `GcRef<T>` for references. Values must be properly rooted across GC boundaries.

4. **Native Functions**: Port native bindings toward the target runtime/VM ABI. Do not add new JS-visible host bindings to legacy crates.
   `otter:kv`, `otter:sql`, and `otter:ffi` now live in `crates/otter-modules`, and `otter:ffi` includes the active `CFunction`, `linkSymbols`, and `JSCallback` path on the new stack.
5. **Types Source Of Truth**: keep Otter `.d.ts` files under `crates/otter-pm/src/types/otter/`; treat `packages/otter-types/` as generated publish output.
6. **Web API Placement**: standards-facing Web API work belongs in `crates/otter-web`, not in `crates/otter-modules`.

7. **Module System**:
   - Must move onto the target runtime integration layer
   - Support for `file://`, `node:`, and `https://` URLs remains required during migration
   - Import maps and graph semantics should be preserved while removing legacy dependencies

8. **Async Operations**: Require Tokio runtime handle. New async/runtime behavior must move toward `otter-runtime` + `otter-vm`, not `otter-vm-runtime`.

9. **Parsing**: Always use ASTs via `oxc` parser. **Never use regex to parse JS/TS code.**

### File Naming for Builtin Modules

| File             | Purpose                                |
|------------------|----------------------------------------|
| `module_ext.rs`  | Rust implementation of native functions |
| `module.js`      | JavaScript shim / polyfills            |

Example: `fs_ext.rs` + `fs.js` for filesystem module.

## Testing

### Unit Tests
```bash
cargo test -p <crate-name>           # Test specific crate
cargo test --all                     # All unit tests
```

### Test262 Conformance
```bash
just test262                         # Run all Test262 tests
just test262-filter "Array"          # Filter by pattern
just test262-dir "language/expressions"  # Specific directory
```

### Node.js Compatibility

`node-compat` is parked while the legacy stack stays frozen. Do not treat the
old Node.js compatibility runner as an active workflow until it is rebuilt on
top of `otter-runtime` + `otter-vm`.

### Test-Driven Development Workflow
When working on features with conformance tests:

1. **Measure before**: Run tests, note the pass rate (e.g., "JSON: 39% passing")
2. **Fix incrementally**: Focus on the most common failure patterns first
3. **Measure after**: Re-run tests, report the delta (e.g., "JSON: 39% → 42.4%")
4. **Run `cargo test` after every change** to core implementations

```bash
# Track progress on a feature
just test262-filter "JSON" 2>&1 | tail -5  # Before
# ... make changes ...
just test262-filter "JSON" 2>&1 | tail -5  # After — compare pass rates
```

## Debugging

### Logging
```bash
RUST_LOG=debug cargo run -p otterjs -- run script.ts
```

### Test262 Watchdog (for hangs)
```bash
cargo run -p otter-test262 -- --filter <pattern> --verbose --timeout 20
```

On timeout, dumps: `stack_depth`, `pc`, `instruction`, `function_name`, `module_url`
- `module_url=setup-builtins-*.js`: Hang in builtin JS shim
- `module_url=main.js`: Hang in test body

### Profiling
```bash
cargo build --release -p otterjs
# Profile with your preferred tool (perf, flamegraph, etc.)
```

## Security Model

Deny-by-default capabilities remain required during migration:
- `fs_read`, `fs_write`: Path allowlists
- `net`: Host allowlists
- `env`: Variable allowlists with secret deny patterns (`AWS_*`, `*_SECRET*`, etc.)
- `subprocess`, `ffi`: Boolean flags

**Never bypass capability checks.** Always enforce at Rust boundary with test coverage, and port the checks to the target stack instead of extending legacy implementations.

## Rust Best Practices

### Collection Types for Deterministic Output
When implementing features that need deterministic output (JSON serializers, iterators, test comparisons):
- **Use ordered collections**: `BTreeMap`, `IndexMap` instead of `HashMap`, `FxHashMap`
- Hash-based maps don't preserve insertion order — this causes flaky tests and non-reproducible output
- Add `indexmap` to `Cargo.toml` when you need both performance and insertion order

```rust
// Bad: non-deterministic key order
use rustc_hash::FxHashMap;
let map: FxHashMap<String, Value> = ...;

// Good: preserves insertion order
use indexmap::IndexMap;
let map: IndexMap<String, Value> = ...;
```

### Recursive Algorithm Safety
Before implementing recursive algorithms (JSON parsing, AST traversal, nested structures):
- Add explicit depth limits to prevent stack overflow
- Consider iterative alternatives with explicit stack for deeply nested inputs
- Test with pathological cases (deeply nested JSON, recursive structures)

```rust
// Bad: unbounded recursion
fn process(value: &Value) -> Result<(), Error> {
    match value {
        Value::Array(arr) => arr.iter().try_for_each(|v| process(v)),
        // ...
    }
}

// Good: depth-limited
fn process(value: &Value, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::MaxDepthExceeded);
    }
    match value {
        Value::Array(arr) => arr.iter().try_for_each(|v| process(v, depth + 1)),
        // ...
    }
}
```

## Development Guidelines

1. **Search first**: Use `rg` to find similar code before adding new patterns
2. **Surgical changes**: Avoid refactors unless requested; keep public APIs stable
3. **Safety boundaries**: Follow `unsafe` rules and GC invariants in AGENTS.md
4. **Update the triangle**: Keep runtime ↔ TypeScript `.d.ts` ↔ tests in sync
5. **AST-first parsing**: Use `oxc` for JS/TS analysis; never regex parsing
6. **Conformance first**: Check `ES_CONFORMANCE.md` before and after feature work. Track pass rate deltas.
7. **Protect the migration boundary**: no new target-stack dependency on legacy crates.

## Current Work

See `NATIVEFN_MIGRATION_REMAINING.md` for ongoing refactor to eliminate `InterceptionSignal` and unify native function callback handling.

## Project References

- `AGENTS.md` - Detailed agent guidance and architecture
- `ROADMAP.md` - Feature status and API compatibility
- `OTTER_VM_PLAN.md` - VM implementation plan
