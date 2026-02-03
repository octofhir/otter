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
cargo test -p otter-vm-core              # VM core tests only
cargo test -p otter-engine               # Engine tests only

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

## Key Architecture

### Crate Layering (bottom-up)

```
otterjs (CLI)
    ↓
otter-engine (Module loading, permissions)
    ↓
otter-vm-runtime (Builtins integration, event loop)
    ↓
otter-vm-core (Interpreter, values, objects, GC)
    ↓
otter-vm-compiler (JS/TS → bytecode)
    ↓
otter-vm-bytecode (Instruction definitions)
    ↓
otter-vm-gc (Garbage collector)
```

### Critical Implementation Details

1. **Value Representation**: NaN-boxing for efficient 64-bit values. See `otter-vm-core/src/value.rs`.

2. **Object Model**: Hidden classes (shapes) for optimized property access. See `otter-vm-core/src/object.rs` and `shape.rs`.

3. **GC Safety**: Use `GcRef<T>` for references. Values must be properly rooted across GC boundaries.

4. **Native Functions**: Use `NativeContext` for calling JS from Rust:
   - Old: `(this, args, mm: Arc<MemoryManager>)`
   - New: `(this, args, ncx: &mut NativeContext<'_>)`
   - Use `ncx.call_function()` to invoke JS callbacks from native code

5. **Module System**:
   - ESM loader in `otter-engine`
   - Support for `file://`, `node:`, and `https://` URLs
   - Import maps for aliasing

6. **Async Operations**: Require Tokio runtime handle (thread-local). Microtasks queue in `otter-vm-runtime`.

7. **Parsing**: Always use ASTs via `oxc` parser. **Never use regex to parse JS/TS code.**

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
```bash
just node-compat                     # Fetch and run Node.js tests
just node-compat-module fs           # Test specific module
just node-compat-status              # Show pass rate
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

Deny-by-default capabilities via `otter-engine`:
- `fs_read`, `fs_write`: Path allowlists
- `net`: Host allowlists
- `env`: Variable allowlists with secret deny patterns (`AWS_*`, `*_SECRET*`, etc.)
- `subprocess`, `ffi`: Boolean flags

**Never bypass capability checks.** Always enforce at Rust boundary with test coverage.

## Development Guidelines

1. **Search first**: Use `rg` to find similar code before adding new patterns
2. **Surgical changes**: Avoid refactors unless requested; keep public APIs stable
3. **Safety boundaries**: Follow `unsafe` rules and GC invariants in AGENTS.md
4. **Update the triangle**: Keep runtime ↔ TypeScript `.d.ts` ↔ tests in sync
5. **AST-first parsing**: Use `oxc` for JS/TS analysis; never regex parsing

## Current Work

See `NATIVEFN_MIGRATION_REMAINING.md` for ongoing refactor to eliminate `InterceptionSignal` and unify native function callback handling.

## Project References

- `AGENTS.md` - Detailed agent guidance and architecture
- `ROADMAP.md` - Feature status and API compatibility
- `OTTER_VM_PLAN.md` - VM implementation plan
