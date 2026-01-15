# otter-pm

NPM-compatible package manager for Otter.

## Overview

`otter-pm` is a lightweight package manager that provides npm registry compatibility for Otter projects. It can download and install packages from the npm registry.

## Features

- Download packages from npm registry
- Resolve dependencies
- Cache packages locally
- Support for semantic versioning

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-pm = "0.1"
```

### Basic Example

```rust
use otter_pm::PackageManager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pm = PackageManager::new()?;

    // Install a package
    pm.install("lodash", "4.17.21").await?;

    Ok(())
}
```

## License

MIT
