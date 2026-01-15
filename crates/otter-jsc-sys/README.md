# otter-jsc-sys

Raw FFI bindings to JavaScriptCore C API.

## Overview

This crate provides low-level unsafe bindings to JavaScriptCore (JSC), the JavaScript engine used by WebKit and Safari. It supports multiple platforms:

- **macOS**: Uses the system JavaScriptCore framework
- **Linux**: Uses pre-built bun-webkit binaries (statically linked)
- **Windows**: Uses pre-built bun-webkit binaries (statically linked)

## Usage

This crate is intended for use by higher-level wrappers. For safe Rust APIs, use the `otter-jsc-core` or `otter-runtime` crates instead.

```rust
use otter_jsc_sys::*;

unsafe {
    let ctx = JSGlobalContextCreate(std::ptr::null_mut());
    // ... use JSC APIs
    JSGlobalContextRelease(ctx);
}
```

## Platform Support

| Platform | Architecture | Method |
|----------|--------------|--------|
| macOS | x86_64, ARM64 | System framework |
| Linux | x86_64, ARM64 | bun-webkit (static) |
| Windows | x86_64 | bun-webkit (static) |

## Build Requirements

### macOS
No additional dependencies required.

### Linux & Windows
The build script automatically downloads pre-built bun-webkit binaries from [oven-sh/WebKit](https://github.com/oven-sh/WebKit).

### Environment Variables

- `BUN_WEBKIT_VERSION`: Override the default bun-webkit version

## License

MIT
