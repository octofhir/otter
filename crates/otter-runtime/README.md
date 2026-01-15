# otter-runtime

TypeScript/JavaScript runtime powered by JavaScriptCore.

## Overview

`otter-runtime` is a high-performance TypeScript and JavaScript execution engine built on JavaScriptCore. It provides a complete runtime environment with:

- TypeScript transpilation (via SWC)
- Async/await and Promise support
- Event loop integration with Tokio
- Module resolution (ESM)

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-runtime = "0.1"
```

### Basic Example

```rust
use otter_runtime::{OtterEngine, Config};

fn main() -> anyhow::Result<()> {
    let engine = OtterEngine::new(Config::default())?;

    // Execute JavaScript
    let result = engine.eval("1 + 2")?;
    println!("Result: {}", result);

    // Execute TypeScript
    let result = engine.eval_typescript(r#"
        const greeting: string = "Hello, World!";
        greeting.toUpperCase();
    "#)?;
    println!("TypeScript result: {}", result);

    Ok(())
}
```

### Async Execution

```rust
use otter_runtime::{OtterEngine, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let engine = OtterEngine::new(Config::default())?;

    let result = engine.eval_async(r#"
        async function fetchData() {
            await new Promise(resolve => setTimeout(resolve, 100));
            return "data loaded";
        }
        fetchData();
    "#).await?;

    println!("Async result: {}", result);
    Ok(())
}
```

## Platform Support

| Platform | Architecture | Status |
|----------|--------------|--------|
| macOS | x86_64, ARM64 | Supported |
| Linux | x86_64, ARM64 | Supported |
| Windows | x86_64 | Supported |

## Features

- `default` - Standard features
- `system-jsc` - Use system JavaScriptCore (macOS only)
- `build-jsc` - Build JavaScriptCore from source

## License

MIT
