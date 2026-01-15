# otter-node

Node.js API compatibility layer for Otter.

## Overview

`otter-node` provides Node.js-compatible APIs for the Otter runtime, enabling existing Node.js code to run with minimal modifications.

## Supported APIs

- `path` - Path manipulation utilities
- `buffer` - Binary data handling
- `fs` - File system operations
- `crypto` - Cryptographic operations (randomBytes, createHash, etc.)
- `stream` - Web Streams API (ReadableStream, WritableStream)
- `websocket` - WebSocket client
- `worker` - Web Worker API
- `test` - Test runner (describe, it, assert)

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-node = "0.1"
```

### Example

```rust
use otter_node::path;
use otter_node::buffer::Buffer;

// Path manipulation
let joined = path::join(&["foo", "bar", "baz.txt"]);
assert_eq!(joined, "foo/bar/baz.txt");

// Buffer operations
let buf = Buffer::from_string("hello", "utf8").unwrap();
assert_eq!(buf.to_string("base64", 0, buf.len()), "aGVsbG8=");
```

## License

MIT
