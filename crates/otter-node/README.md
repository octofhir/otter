# otter-node

Node.js API compatibility layer for Otter.

## Overview

`otter-node` provides Node.js-compatible APIs for the Otter runtime, enabling existing Node.js code to run with minimal modifications.

## Supported APIs

- `fs` - File system operations
- `path` - Path manipulation
- `buffer` - Buffer handling
- `crypto` - Cryptographic operations

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-node = "0.1"
```

## License

MIT
