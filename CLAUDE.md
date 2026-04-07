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

Current fast-path CLI surface:
- enabled: `run`, direct file execution, `-e`, `-p`, package-management commands
- disabled: `repl`, `test`, `build`

## Key Architecture

Current runtime stack:
- `crates/otter-gc`
- `crates/otter-vm`
- `crates/otter-runtime`
- `crates/otter-jit`

Compatibility rules:
- New runtime/VM/API work belongs on the current runtime stack.
- Do not add new dependencies from active crates into parked compatibility shims.
- Keep `otter-nodejs` and `otter-node-compat` compileable, but treat them as parked surfaces rather than active implementation homes.

### Crate Layering (bottom-up)

```
otterjs (CLI)
    ↓
    host/runtime integration layer
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

4. **Native Functions**: Add native bindings against the current runtime/VM ABI.
   `otter:kv`, `otter:sql`, and `otter:ffi` live in `crates/otter-modules`, and `otter:ffi` includes the active `CFunction`, `linkSymbols`, and `JSCallback` path there.
   Prefer the active macro set when it matches the surface:
   - `#[js_class]` for classes
   - `#[js_namespace]` for namespaces
   - `#[dive]` for one binding
     `#[dive]` is sync by default; `#[dive(deep)]` is the async variant of the same macro
   - `raft!` for grouped target bindings
   - `burrow!` for host-owned object surfaces
   - `lodge!` for hosted module loaders
5. **Types Source Of Truth**: keep Otter `.d.ts` files under `crates/otter-pm/src/types/otter/`; treat `packages/otter-types/` as generated publish output.
6. **Web API Placement**: standards-facing Web API work belongs in `crates/otter-web`, not in `crates/otter-modules`.

7. **Module System**:
   - Lives on the runtime integration layer
   - Support for `file://`, `node:`, and `https://` URLs remains required
   - Import maps and graph semantics should be preserved as the runtime surface evolves

8. **Async Operations**: Require Tokio runtime handle.

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

`node-compat` is parked. Do not treat the old Node.js compatibility runner as
an active workflow until it is rebuilt on top of `otter-runtime` + `otter-vm`.

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

Deny-by-default capabilities remain required:
- `fs_read`, `fs_write`: Path allowlists
- `net`: Host allowlists
- `env`: Variable allowlists with secret deny patterns (`AWS_*`, `*_SECRET*`, etc.)
- `subprocess`, `ffi`: Boolean flags

**Never bypass capability checks.** Always enforce at the Rust boundary with test coverage.

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
7. **Protect active boundaries**: no new active-runtime dependency on parked compatibility shims.

## Macro Rules

- Treat `crates/otter-macros/README.md` as the user-facing source of truth for macro usage and examples.
- Prefer descriptor-driven macros over manual boilerplate when the surface clearly matches one of the active macros.
- Keep macro use explicit: JS names, arity, and export shape should remain obvious in the declaration site.
- Keep code manual when capability checks, complex runtime sequencing, or non-obvious installation order matter more than boilerplate reduction.

## Current Work

See `NATIVEFN_MIGRATION_REMAINING.md` for ongoing refactor to eliminate `InterceptionSignal` and unify native function callback handling.

## Project References

- `AGENTS.md` - Detailed agent guidance and architecture
- `ROADMAP.md` - Feature status and API compatibility
- `OTTER_VM_PLAN.md` - VM implementation plan
