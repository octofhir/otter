# Otter

Embeddable TypeScript/JavaScript runtime and CLI powered by a custom bytecode VM.

## Overview

Otter is designed to embed scripting in Rust applications or run scripts directly from the CLI:

1. **Embeddable engine** - Use `otter-engine` (high-level) or `otter-vm-runtime` (lower-level)
2. **Standalone CLI** - Run scripts with the `otter` binary (`otterjs` crate)

The runtime is a custom VM with garbage collection, written in Rust. This is a new runtime under active development — we are continuously adding features and compatibility, and APIs may evolve between releases.

## Installation

### As a Rust library

```toml
[dependencies]
otter-engine = "0.1"
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

### Permissions

Deny-by-default capability system:

```bash
otter run app.ts --allow-read           # file system read
otter run app.ts --allow-write          # file system write
otter run app.ts --allow-net            # network access
otter run app.ts --allow-env            # environment variables
otter run app.ts --allow-all            # all permissions
```

## Embedding in Rust

```rust
use otter_engine::{CapabilitiesBuilder, EngineBuilder};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut engine = EngineBuilder::new()
        .capabilities(CapabilitiesBuilder::new().allow_net_all().build())
        .with_http() // Enable Otter.serve()
        .build();

    engine
        .eval(r#"
            const res = await fetch("https://example.com");
            console.log(res.status);
        "#)
        .await?;

    Ok(())
}
```

## Runtime Features (current)

- Custom bytecode VM + JS/TS compiler with TypeScript support out of the box
- ESM and CommonJS module loader with resolution and import maps
- Remote modules over `https://` with allowlist-based security
- Capability-based permissions for `read`, `write`, `net`, and `env`
- Web-compatible `fetch` with `Headers`, `Request`, and `Response`
- Optional HTTP server API: `Otter.serve()` (enabled via `EngineBuilder::with_http()` and in the CLI)
- Core JavaScript builtins (Object/Array/Map/Set/Date/RegExp/JSON/Promise/Proxy/Reflect/Symbol, etc.)
- Extension system for native ops + JS shims

## Status

Otter is actively evolving. The package ecosystem support is in progress (see `crates/otter-pm`). Expect regular additions to the runtime surface — follow `ROADMAP.md` for planned work.

## Project Structure

```text
crates/
├── otter-vm-bytecode  # Bytecode definitions
├── otter-vm-gc        # Garbage collector
├── otter-vm-core      # VM interpreter
├── otter-vm-compiler  # JS/TS to bytecode compiler
├── otter-vm-runtime   # Runtime with builtins
├── otter-vm-builtins  # Built-in functions and JS shims
├── otter-engine       # Module loader, capabilities, extensions
├── otter-pm           # Package manager integration (in progress)
├── otter-sql          # SQL extension (SQLite + PostgreSQL)
├── otter-kv           # Key-value store extension
├── otter-profiler     # Runtime profiler
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
