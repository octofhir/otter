# otter-runtime

TypeScript/JavaScript runtime powered by JavaScriptCore.

## Overview

`otter-runtime` is a high-performance TypeScript and JavaScript execution engine built on JavaScriptCore. It provides a complete runtime environment with:

- TypeScript transpilation (via SWC)
- Async/await and Promise support
- Event loop with timers
- `fetch` API with Headers, Request, Response
- Console API with customizable handlers

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-runtime = "0.1"
```

### Basic Example

```rust
use otter_runtime::{JscRuntime, JscConfig};

fn main() -> anyhow::Result<()> {
    let runtime = JscRuntime::new(JscConfig::default())?;

    // Execute JavaScript
    let result = runtime.eval("1 + 2")?;
    println!("Result: {}", result.to_number()?);

    // Execute TypeScript
    let result = runtime.eval(r#"
        const greeting: string = "Hello, World!";
        greeting.toUpperCase();
    "#)?;
    println!("TypeScript result: {}", result.to_string()?);

    Ok(())
}
```

### With Custom Console Handler

```rust
use otter_runtime::{JscRuntime, JscConfig, ConsoleLevel, set_console_handler};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    // Custom console output handler
    set_console_handler(|level, message| match level {
        ConsoleLevel::Error | ConsoleLevel::Warn => eprintln!("{}", message),
        _ => println!("{}", message),
    });

    let runtime = JscRuntime::new(JscConfig::default())?;

    runtime.eval(r#"
        console.log("Hello from JS!");
        console.error("This is an error");
    "#)?;

    // Run event loop for async operations
    runtime.run_event_loop_until_idle(Duration::from_millis(5000))?;

    Ok(())
}
```

### Runtime Pool for Concurrent Execution

```rust
use otter_runtime::{JscRuntimePool, JscConfig};

fn main() -> anyhow::Result<()> {
    let config = JscConfig {
        pool_size: 4,
        timeout_ms: 5000,
        enable_console: true,
    };

    let pool = JscRuntimePool::new(config)?;

    let result = pool.eval("2 + 2")?;
    println!("Result: {}", result.to_number()?);

    Ok(())
}
```

### Engine API with Tokio

The `Engine` API provides a thread-pool based execution model that requires a Tokio runtime for async operations like `fetch()`.

```rust
use otter_runtime::Engine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Engine automatically captures the current Tokio handle
    let engine = Engine::new()?;
    let handle = engine.handle();

    // Evaluate JavaScript
    let result = handle.eval("1 + 1").await?;
    println!("Result: {}", result);

    // Or provide a custom Tokio handle
    let custom_handle = tokio::runtime::Handle::current();
    let engine = Engine::builder()
        .pool_size(4)
        .tokio_handle(custom_handle)
        .build()?;

    engine.shutdown().await;
    Ok(())
}
```

**Note:** The `Engine` must be created within a Tokio runtime context. It automatically captures the current Tokio handle via `Handle::current()`, which is required for async operations in worker threads.

## Platform Support

| Platform | Architecture | Status |
|----------|--------------|--------|
| macOS | x86_64, ARM64 | Supported |
| Linux | x86_64, ARM64 | Supported |
| Windows | x86_64 | Supported |

## License

MIT
