# Otter

Embeddable TypeScript/JavaScript runtime and CLI powered by a custom bytecode VM.

## Overview

Otter is designed to embed scripting in Rust applications or run scripts directly from the CLI:

1. **Embeddable runtime** - Use `otter-runtime` as the active public embedding API
2. **Standalone CLI** - Run scripts with the `otter` binary (`otterjs` crate)

The runtime is a custom VM with garbage collection, written in Rust. The current core crates are `otter-gc` + `otter-vm` + `otter-runtime`, with `otter-jit` as the active JIT pipeline.

## Installation

### As a Rust library

```toml
[dependencies]
otter-runtime = "0.1"
```

### As a CLI

```bash
cargo install otterjs
```

## CLI Usage

```bash
# Run a file
otter run app.ts
otter app.ts                    # shorthand

# Evaluate inline code
otter -e "console.log('hi')"

# Runtime info and init
otter info
otter init
```

The current CLI intentionally keeps a smaller active surface. `repl`, `test`, and `build` are not active commands at this stage.

### Permissions

Capability and host-integration work is still evolving. Expect this surface to keep growing as more hosted functionality lands directly on `otter-runtime`.

```bash
otter run app.ts --allow-read           # file system read
otter run app.ts --allow-write          # file system write
otter run app.ts --allow-net            # network access
otter run app.ts --allow-env            # environment variables
otter run app.ts --allow-all            # all permissions
```

## Embedding in Rust

```rust
use otter_runtime::OtterRuntime;

fn main() -> anyhow::Result<()> {
    let mut rt = OtterRuntime::builder().build();
    rt.run_script("console.log('hello from otter-runtime')", "main.js")?;
    Ok(())
}
```

## Runtime Features (current)

- Custom bytecode VM + JS/TS compiler with TypeScript support out of the box
- Module/runtime host features live on `otter-runtime`
- Capability-based permissions remain a core design requirement
- Web/API and extension surfaces are being expanded incrementally
- Standards-facing Web APIs now land in `crates/otter-web`, with `TextEncoder`, `TextDecoder`, `URL`, `URLSearchParams`, and `Headers` already active
- Active otter-specific hosted modules now live in `crates/otter-modules`, including `otter:kv`, `otter:sql`, and `otter:ffi`
- Core JavaScript builtins (Object/Array/Map/Set/Date/RegExp/JSON/Promise/Proxy/Reflect/Symbol, etc.)
- Test262 runner is active on the current runtime stack

## Status

Otter is actively evolving. Package ecosystem support is in progress (see `crates/otter-pm`). Expect regular additions to the runtime surface as more host features land on `otter-runtime`.

## Project Structure

```text
crates/
├── otter-gc           # Active garbage collector
├── otter-vm           # Active VM, compiler, intrinsics
├── otter-runtime      # Active public runtime API
├── otter-jit          # Active JIT pipeline
├── otter-modules      # Active otter:* hosted modules (kv/sql/ffi)
├── otter-web          # Active Web API crate
├── otter-pm           # Package manager integration (in progress)
├── otter-test262      # Active conformance runner
├── otter-nodejs       # Parked Node.js compatibility shim
├── otter-node-compat  # Parked node-compat shim
└── otterjs            # CLI binary
```

## Development

```bash
cargo build                              # debug build
cargo build --release -p otterjs         # release CLI
cargo test --all                         # run tests
cargo run -p otterjs -- run examples/basic.ts
```

## License

MIT
