# otter-engine

ESM module loader and capability-based security for Otter.

## Overview

`otter-engine` provides the module loading infrastructure for the Otter runtime:

- ESM module loading from file://, node:, and https:// URLs
- Dependency graph with cycle detection
- Capability-based security model
- Automatic TypeScript transpilation
- Import maps support

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-engine = "0.1"
```

### Module Loading

```rust
use otter_engine::{ModuleLoader, ModuleGraph, LoaderConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = LoaderConfig::default();
    let loader = Arc::new(ModuleLoader::new(config));
    let mut graph = ModuleGraph::new(loader);

    // Load a module and its dependencies
    graph.load("file:///path/to/main.ts").await?;

    // Get execution order (topologically sorted)
    for url in graph.execution_order() {
        println!("Execute: {}", url);
    }

    Ok(())
}
```

### Capability-Based Security

```rust
use otter_engine::{Capabilities, CapabilitiesBuilder};
use std::path::PathBuf;

let caps = CapabilitiesBuilder::new()
    .allow_read([PathBuf::from("/app/src")])
    .allow_net(["api.example.com".to_string()])
    .allow_env(["NODE_ENV".to_string()])
    .build();

// Check permissions
if caps.can_read("/app/src/main.ts") {
    // allowed
}

// Or require permission (returns error if denied)
caps.require_net("api.example.com")?;
```

## License

MIT
