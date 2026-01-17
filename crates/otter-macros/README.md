# otter-macros

Proc-macros for the Otter runtime.

## Overview

`otter-macros` provides procedural macros for creating native functions callable from JavaScript in the Otter runtime. The main export is the `#[dive]` attribute macro.

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-macros = "0.1"
```

### The `#[dive]` Attribute

The `#[dive]` attribute marks a Rust function as callable from JavaScript. The name comes from the way otters dive for fish - our functions "dive" into native code to fetch results.

#### Modes

- `#[dive]` or `#[dive(swift)]` - Synchronous function, returns value directly
- `#[dive(deep)]` - Async function, returns a Promise that resolves when complete

#### Example

```rust
use otter_macros::dive;

#[dive(swift)]  // Quick synchronous operation
fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[dive(deep)]  // Async operation - returns Promise
async fn fetch_data(url: String) -> Result<String, Error> {
    // ... async implementation
}
```

### Supported Types

Arguments and return types must implement `serde::Serialize` and `serde::Deserialize`. Common types that work out of the box:

- Primitives: `i32`, `i64`, `f64`, `bool`, `String`
- Collections: `Vec<T>`, `HashMap<K, V>`
- Options: `Option<T>`
- Custom types with `#[derive(Serialize, Deserialize)]`

### Generated Code

The macro generates:

1. The original function (unchanged)
2. A `{name}_dive_decl()` function returning `OpDecl` for registration

```rust
// This:
#[dive(swift)]
fn add(a: i32, b: i32) -> i32 { a + b }

// Generates:
fn add(a: i32, b: i32) -> i32 { a + b }

pub fn add_dive_decl() -> otter_runtime::extension::OpDecl {
    otter_runtime::extension::op_sync("add", |_ctx, args| {
        // argument extraction and function call
    })
}
```

## Otter Terminology

| Term | Meaning |
|------|---------|
| **dive** | A native function (otters dive for fish) |
| **swift** | Fast synchronous dive |
| **deep** | Async dive that goes "deeper" and returns a Promise |

## License

MIT
