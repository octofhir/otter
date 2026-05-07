# Otter

Embeddable TypeScript/JavaScript runtime and CLI powered by a custom bytecode VM.

## Overview

Otter is designed to embed scripting in Rust applications or run scripts directly from the CLI:

1. **Embeddable runtime** - Use `otter-runtime` as the active public embedding API
2. **Standalone CLI** - Run scripts with the `otter` binary (`otter-cli` crate)

The runtime is a custom VM with garbage collection, written in Rust. The current core crates are `otter-gc` + `otter-vm` + `otter-runtime`.

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
use otter_runtime::{Runtime, SourceInput};

fn main() -> anyhow::Result<()> {
    let mut rt = Runtime::builder().build()?;
    rt.run_script(
        SourceInput::from_javascript("console.log('hello from otter-runtime')"),
        "main.js",
    )?;
    Ok(())
}
```

## Runtime Features (current)

- Custom bytecode VM + JS/TS compiler with TypeScript support out of the box
- Module/runtime host features live on `otter-runtime`
- Capability-based permissions remain a core design requirement
- Standards-facing Web APIs live in `crates/otter-web`, with host-side slices for `URL`, `Headers`, `Blob`, `Request`, and `Response`
- Otter-specific hosted modules live in `crates/otter-modules`, including importable `otter:kv`, `otter:sql`, and `otter:ffi`
- Core JavaScript builtins (Object/Array/Map/Set/Date/RegExp/JSON/Promise/Proxy/Reflect/Symbol, etc.)
- Test262 runner is active on the current runtime stack

## Status

Otter is actively evolving. Expect regular additions to the runtime surface as more host features land on `otter-runtime`.

## Project Structure

```text
crates/
├── otter-gc           # Active garbage collector
├── otter-vm           # Active VM, compiler, intrinsics
├── otter-runtime      # Active public runtime API
├── otter-modules      # Active otter:* hosted modules (kv/sql/ffi)
├── otter-web          # Active Web API crate
├── otter-test262      # Active conformance runner
└── otter-cli          # CLI binary
```

## Development

```bash
cargo build                              # debug build
cargo build --release -p otter-cli       # release CLI
cargo test --all                         # run tests
cargo run -p otter-cli -- run examples/basic.ts
```

## License

MIT
