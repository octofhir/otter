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
- HTTP/HTTPS server with HTTP/1.1 + HTTP/2 support
- Built-in SQLite & PostgreSQL with tagged template queries
- Key-value store (redb)
- Console API with customizable output handlers
- Timeout control for script execution
- Cross-platform: macOS, Linux, Windows

## Performance

| Runtime | Cold Start | Warm Start |
|---------|------------|------------|
| **Otter** | ~0.03s | **0.01s** |
| Bun | 0.01s | 0.01s |
| Node.js | 0.06s | 0.03s |

Key optimizations: event loop idle detection, lazy extension loading, LTO builds.
Binary size: 38MB.

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
| `assert` | ✅ Full | `AssertionError`, `ok`, `equal`, `deepEqual`, `strictEqual`, `throws`, `rejects` |
| `buffer` | ✅ Full | `Buffer.alloc`, `from`, `concat`, `slice`, `toString`, `isBuffer`, `byteLength` |
| `child_process` | ✅ Full | `spawn`, `spawnSync`, `exec`, `execSync`, `execFile`, `execFileSync`, `fork` |
| `crypto` | ⚠️ Partial | `randomBytes`, `randomUUID`, `createHash`, `createHmac`, `hash` |
| `dgram` | ✅ Full | `createSocket`, `bind`, `send`, `close` (UDP sockets) |
| `dns` | ✅ Full | `lookup`, `resolve`, `resolve4`, `resolve6` (hickory-resolver) |
| `events` | ✅ Full | `EventEmitter` with full API (on, once, emit, off, etc.) |
| `fs` | ✅ Full | `readFile`, `writeFile`, `readdir`, `stat`, `mkdir`, `rm`, `exists`, `rename`, `copyFile` + sync + promises |
| `os` | ✅ Full | `arch`, `platform`, `hostname`, `homedir`, `tmpdir`, `cpus`, `totalmem`, `freemem`, `userInfo`, etc. |
| `path` | ✅ Full | `join`, `resolve`, `dirname`, `basename`, `extname`, `normalize`, `isAbsolute`, `relative` |
| `process` | ✅ Full | `env`, `argv`, `cwd`, `exit`, `memoryUsage`, `platform`, `arch` |
| `querystring` | ✅ Full | `parse`, `stringify`, `encode`, `decode` |
| `test` | ✅ Full | `describe`, `it`, `test`, `run` (node:test compatible) |
| `url` | ✅ Full | WHATWG URL + legacy `parse`, `format`, `resolve` |
| `util` | ⚠️ Partial | `promisify`, `inspect`, `format` |
| `zlib` | ✅ Full | `gzip`, `gunzip`, `deflate`, `inflate`, `brotliCompress`, `brotliDecompress` + sync |
| `http`/`https` | ❌ | Use `fetch` or `Otter.serve()` instead |
| `net` | ⚠️ Partial | TCP partial, use `dgram` for UDP |

### Otter-specific APIs

| API | Description |
|-----|-------------|
| `Otter.serve()` | HTTP/HTTPS server (HTTP/1.1 + HTTP/2 with ALPN, Bun-compatible) |
| `Otter.spawn()` | Async subprocess with ReadableStream stdout/stderr |
| `Otter.spawnSync()` | Synchronous subprocess execution |

### Database & Storage

| API | Status | Description |
|-----|--------|-------------|
| `SQL` | ✅ Full | SQLite + PostgreSQL with tagged template queries |
| `sql` | ✅ Full | Bun-compatible tagged template literal for queries |
| `kv()` | ✅ Full | Key-value store (redb, pure Rust) |

```typescript
import { sql, SQL } from "otter";

// SQLite (default)
const db = new SQL(":memory:");
await db`CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)`;
await db`INSERT INTO users (name) VALUES (${"Alice"})`;
const users = await db`SELECT * FROM users`;

// PostgreSQL with COPY (bulk import/export)
const pg = new SQL("postgres://user:pass@localhost/db");
await pg.copyFrom("users", {
  columns: ["name", "email"],
  format: "csv",
  source: new Blob(["Alice,alice@example.com\nBob,bob@example.com"]),
});
// 108,529 rows/sec - 144x faster than single INSERTs

// KV Store
import { kv } from "otter";
const store = kv(":memory:");
store.set("user:1", { name: "Alice" });
console.log(store.get("user:1")); // { name: "Alice" }
```

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
