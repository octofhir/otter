# otter-jsc-core

Safe Rust wrappers for JavaScriptCore.

## Overview

This crate provides safe, ergonomic Rust bindings to JavaScriptCore (JSC). It wraps the low-level FFI bindings from `otter-jsc-sys` with proper memory management and error handling.

## Features

- Safe context and value management
- Automatic memory management with RAII
- JSON serialization/deserialization
- Function callbacks from JavaScript to Rust
- Property access and manipulation

## Usage

```rust
use otter_jsc_core::{JscContext, JscValue};

fn main() -> Result<(), otter_jsc_core::JscError> {
    let ctx = JscContext::new()?;

    // Evaluate JavaScript
    let result = ctx.eval("1 + 2")?;
    println!("Result: {}", result.to_number()?);

    // Create values
    let obj = ctx.create_object()?;
    obj.set_property("name", ctx.create_string("Otter")?)?;

    Ok(())
}
```

## Platform Support

See `otter-jsc-sys` for platform support details.

## License

MIT
