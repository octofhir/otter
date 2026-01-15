# otter-pm

NPM-compatible package manager for Otter.

## Overview

`otter-pm` provides npm registry client, dependency resolution, and package installation for Otter projects.

## Features

- Download packages from npm registry
- Dependency resolution with semver
- Lockfile support
- Local package cache

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-pm = "0.1"
```

### Example

```rust
use otter_pm::{Installer, NpmRegistry, Resolver};
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let registry = NpmRegistry::new();

    // Fetch package metadata
    let metadata = registry.get_package("lodash").await?;
    println!("Latest version: {}", metadata.dist_tags.get("latest").unwrap());

    // Install dependencies from package.json
    let installer = Installer::new(Path::new("."));
    installer.install().await?;

    Ok(())
}
```

## License

MIT
