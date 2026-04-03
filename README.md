# Otter

Embeddable TypeScript/JavaScript runtime and CLI powered by a custom bytecode VM.

## Overview

Otter is designed to embed scripting in Rust applications or run scripts directly from the CLI:

1. **Embeddable runtime** - Use `otter-runtime` as the active public embedding API
2. **Standalone CLI** - Run scripts with the `otter` binary (`otterjs` crate)

The runtime is a custom VM with garbage collection, written in Rust. The active stack is `otter-gc` + `otter-vm` + `otter-runtime`; older engine crates are frozen while functionality is ported over.

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

The current fast-path CLI intentionally keeps a smaller active surface during migration. `repl`, `test`, and `build` are not active commands at this stage.

### Permissions

Capability and host-integration work is being ported to the active runtime stack. Expect this surface to keep evolving while the legacy stack stays frozen.

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
- Module/runtime host features are being ported onto `otter-runtime`
- Capability-based permissions remain a design requirement during migration
- Web/API and extension surfaces are being reintroduced incrementally on the active stack
- Core JavaScript builtins (Object/Array/Map/Set/Date/RegExp/JSON/Promise/Proxy/Reflect/Symbol, etc.)
- Test262 runner is active on the new runtime stack

## Status

Otter is actively evolving. The package ecosystem support is in progress (see `crates/otter-pm`). Expect regular additions to the runtime surface as deferred host features are ported to `otter-runtime`.

## Project Structure

```text
crates/
├── otter-gc           # Active garbage collector
├── otter-vm           # Active VM, compiler, intrinsics
├── otter-runtime      # Active public runtime API
├── otter-pm           # Package manager integration (in progress)
├── otter-test262      # Active conformance runner
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
