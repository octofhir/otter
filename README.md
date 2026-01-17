# Otter

![Otter Logo](./otter-logo.png)

**Embeddable TypeScript/JavaScript engine for Rust applications.**

Otter is designed primarily as a library for embedding scripting capabilities into Rust applications. Built on JavaScriptCore with native TypeScript support, async runtime, and a minimal footprint.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
otter-runtime = "0.1"
```

## Embedding in Rust

```rust
use otter_runtime::{ConsoleLevel, JscConfig, JscRuntime, set_console_handler};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    // Optional: custom console output handler
    set_console_handler(|level, message| match level {
        ConsoleLevel::Error | ConsoleLevel::Warn => eprintln!("{}", message),
        _ => println!("{}", message),
    });

    let runtime = JscRuntime::new(JscConfig::default())?;

    // Execute TypeScript/JavaScript
    runtime.eval(r#"
        interface User { name: string; age: number }
        const user: User = { name: "Alice", age: 30 };
        console.log(JSON.stringify(user));
    "#)?;

    // Run event loop for async operations
    runtime.run_event_loop_until_idle(Duration::from_millis(5000))?;

    Ok(())
}
```

## Features

- Native TypeScript support (no separate compilation step)
- Async/await with built-in event loop
- `fetch` API with Headers, Request, Response, Blob, FormData
- Console API with customizable output handlers
- Timeout control for script execution
- Cross-platform: macOS, Linux, Windows

## API Compatibility

### Web APIs

| API | Status | Notes |
|-----|--------|-------|
| `fetch` | ✅ Full | Headers, Request, Response, Blob, FormData |
| `console` | ✅ Full | log, error, warn, info, debug, trace, time/timeEnd |
| `setTimeout/setInterval` | ✅ Full | + clearTimeout, clearInterval, setImmediate |
| `AbortController/AbortSignal` | ✅ Full | Cancellation API |
| `EventTarget/Event` | ✅ Full | DOM-style events |
| `URL/URLSearchParams` | ✅ Full | WHATWG URL Standard |
| `TextEncoder/TextDecoder` | ✅ Full | UTF-8 encoding |
| `ReadableStream/WritableStream` | ✅ Full | WHATWG Streams |
| `WebSocket` | ✅ Full | RFC 6455 |
| `Worker` | ✅ Full | Web Workers |
| `performance.now()` | ✅ Full | High-resolution timing |
| `crypto.getRandomValues` | ✅ Full | Web Crypto (partial) |

### Node.js Modules

| Module | Status | Implemented APIs |
|--------|--------|------------------|
| `buffer` | ✅ Full | `Buffer.alloc`, `from`, `concat`, `slice`, `toString`, `isBuffer`, `byteLength` |
| `child_process` | ✅ Full | `spawn`, `spawnSync`, `exec`, `execSync`, `execFile`, `execFileSync`, `fork` |
| `crypto` | ⚠️ Partial | `randomBytes`, `randomUUID`, `createHash`, `createHmac`, `hash` |
| `events` | ✅ Full | `EventEmitter` with full API (on, once, emit, off, etc.) |
| `fs` | ✅ Full | `readFile`, `writeFile`, `readdir`, `stat`, `mkdir`, `rm`, `exists`, `rename`, `copyFile` + sync + promises |
| `os` | ✅ Full | `arch`, `platform`, `hostname`, `homedir`, `tmpdir`, `cpus`, `totalmem`, `freemem`, `userInfo`, etc. |
| `path` | ✅ Full | `join`, `resolve`, `dirname`, `basename`, `extname`, `normalize`, `isAbsolute`, `relative` |
| `process` | ✅ Full | `env`, `argv`, `cwd`, `exit`, `memoryUsage`, `platform`, `arch` |
| `test` | ✅ Full | `describe`, `it`, `test`, `run` (node:test compatible) |
| `util` | ⚠️ Partial | `promisify`, `inspect`, `format` |
| `dns` | ❌ | Not yet implemented |
| `http`/`https` | ❌ | Use `fetch` or `Otter.serve()` instead |
| `net` | ❌ | TCP/UDP sockets not yet implemented |
| `zlib` | ❌ | Not yet implemented |

### Otter-specific APIs

| API | Description |
|-----|-------------|
| `Otter.serve()` | HTTP/HTTPS server (HTTP/1.1 + HTTP/2 with ALPN, Bun-compatible) |
| `Otter.spawn()` | Async subprocess with ReadableStream stdout/stderr |
| `Otter.spawnSync()` | Synchronous subprocess execution |

### Module System

- ✅ ES Modules (import/export)
- ✅ CommonJS (require/module.exports)
- ✅ TypeScript (.ts, .tsx)
- ✅ Import maps
- ✅ `node:*` built-in module specifiers

### Security (Permissions)

Capability-based, deny-by-default:
- `--allow-read` - File system read
- `--allow-write` - File system write
- `--allow-net` - Network access
- `--allow-env` - Environment variables (with automatic secret filtering)
- `--allow-run` - Subprocess execution
- `--allow-all` - All permissions

## CLI (Optional)

Otter also provides a standalone CLI for running scripts directly:

```bash
# Install
cargo install otter-cli

# Run scripts
otter run script.ts
otter script.js --timeout-ms 5000
```

## CLI Usage

```bash
cargo run -p otter-cli -- run <path/to/script.js> --timeout-ms 5000
```

`console.log` maps to stdout, `console.error`/`console.warn` to stderr in the CLI.
Use `--timeout-ms 0` to disable the timeout.

## Examples

```bash
cargo run -p otter-cli -- run examples/event_loop.js
cargo run -p otter-cli -- run examples/http_fetch.js
cargo run -p otter-cli -- run examples/fetch_webapi.js
```

## Platform Support

| Platform | Architecture | Method |
|----------|--------------|--------|
| macOS | x86_64, ARM64 | System JavaScriptCore |
| Linux | x86_64, ARM64 | Pre-built bun-webkit |
| Windows | x86_64 | Pre-built bun-webkit |

## License

MIT
