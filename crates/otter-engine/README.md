# otter-engine

High-level JavaScript/TypeScript execution engine for Otter.

## Overview

`otter-engine` provides the high-level API for executing JavaScript and TypeScript code with Otter. It handles module resolution, file loading, and orchestrates the runtime.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-engine = "0.1"
```

### Basic Example

```rust
use otter_engine::Engine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = Engine::new()?;

    // Run a TypeScript file
    engine.run_file("src/index.ts").await?;

    Ok(())
}
```

## License

MIT
