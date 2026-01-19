# Otter

TypeScript/JavaScript runtime built on JavaScriptCore.

## Overview

Otter is a TypeScript/JavaScript runtime that can be used as:

1. **Embeddable engine** - Add scripting to Rust applications via `otter-runtime` crate
2. **Standalone CLI** - Run scripts directly with `otter run script.ts`

Built on JavaScriptCore (JSC) from WebKit. Uses bun-webkit for JIT compilation on all platforms.

## Installation

### As Rust Library

```toml
[dependencies]
otter-runtime = "0.1"
```

### As CLI

```bash
cargo install otterjs
```

## CLI Usage

```bash
# Run a file
otter run app.ts
otter app.ts                    # shorthand

# Run package.json script
otter run build                 # runs "build" script
otter run test -- --coverage    # pass args to script

# Execute package binary (like npx)
otter x cowsay "Hello"          # downloads if needed
otter x typescript --help       # uses local node_modules/.bin first
otter x -y esbuild@0.19 app.ts  # specific version, skip prompt

# Build & compile
otter build app.ts -o bundle.js       # bundle to JS
otter build app.ts --compile -o myapp # compile to standalone executable
./myapp                               # runs without otter installed

# Other commands
otter check app.ts              # type check with tsgo
otter test                      # run tests
otter repl                      # interactive REPL
otter install                   # install dependencies
```

### Permissions

Deny-by-default capability system:

```bash
otter run app.ts --allow-read           # file system read
otter run app.ts --allow-write          # file system write
otter run app.ts --allow-net            # network access
otter run app.ts --allow-env            # environment variables
otter run app.ts --allow-run            # subprocess execution
otter run app.ts --allow-all            # all permissions
```

Environment variables have automatic filtering for secrets (AWS_*, *_SECRET*, etc.).

## Embedding in Rust

```rust
use otter_runtime::{JscConfig, JscRuntime, set_console_handler, ConsoleLevel};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    set_console_handler(|level, message| match level {
        ConsoleLevel::Error | ConsoleLevel::Warn => eprintln!("{}", message),
        _ => println!("{}", message),
    });

    let runtime = JscRuntime::new(JscConfig::default())?;

    runtime.eval(r#"
        interface User { name: string; age: number }
        const user: User = { name: "Alice", age: 30 };
        console.log(JSON.stringify(user));
    "#)?;

    runtime.run_event_loop_until_idle(Duration::from_millis(5000))?;
    Ok(())
}
```

## API Support

### Web APIs

- `fetch`, `Headers`, `Request`, `Response`, `Blob`, `FormData`
- `console` (log, error, warn, info, debug, trace, time/timeEnd)
- `setTimeout`, `setInterval`, `setImmediate`, `clearTimeout`, `clearInterval`
- `AbortController`, `AbortSignal`
- `EventTarget`, `Event`
- `URL`, `URLSearchParams`
- `TextEncoder`, `TextDecoder`
- `ReadableStream`, `WritableStream`
- `WebSocket`
- `Worker`
- `performance.now()`
- `crypto.getRandomValues`, `crypto.randomUUID`

### Node.js Modules

| Module | Status | Notes |
|--------|--------|-------|
| `assert` | ✅ Full | 98% - missing CallTracker |
| `async_hooks` | ⚠️ Partial | 60% - AsyncLocalStorage works |
| `buffer` | ✅ Full | 100% - all read/write methods, File, Blob |
| `child_process` | ✅ Full | 95% - spawn, exec, fork with IPC |
| `crypto` | ✅ Full | 100% - hash, hmac, KDFs, ciphers, sign/verify, keypair, webcrypto full |
| `dgram` | ✅ Full | 85% - UDP sockets |
| `dns` | ✅ Full | 70% - hickory-resolver |
| `events` | ✅ Full | 95% - EventEmitter |
| `fs` | ✅ Full | 55% - sync + promises, missing watch/streams |
| `http`/`https` | ✅ Full | 80% - createServer, request, get |
| `net` | ⚠️ Partial | 50% - TCP client/server |
| `os` | ✅ Full | 85% - all main APIs |
| `path` | ✅ Full | 100% |
| `process` | ✅ Full | 85% |
| `querystring` | ✅ Full | 100% |
| `readline` | ⚠️ Partial | 70% - missing completer |
| `stream` | ✅ Full | 95% - Readable, Writable, Transform, pipeline |
| `string_decoder` | ✅ Full | 100% |
| `test` | ✅ Full | 80% - node:test compatible |
| `timers` | ✅ Full | 100% - timers + timers/promises |
| `tty` | ⚠️ Partial | 30% - isatty |
| `url` | ✅ Full | 100% - WHATWG + legacy |
| `util` | ⚠️ Partial | 60% - promisify, inspect, format, types |
| `zlib` | ✅ Full | 98% - gzip, deflate, brotli |

**Not yet implemented:** `cluster`, `worker_threads`, `tls`, `vm`, `perf_hooks`, `inspector`

### Otter APIs

```typescript
// HTTP server (Bun-compatible API)
Otter.serve({
  port: 3000,
  fetch(req) {
    return new Response("Hello");
  }
});

// Subprocess with streams
const proc = Otter.spawn(["ls", "-la"]);
for await (const chunk of proc.stdout) {
  console.log(new TextDecoder().decode(chunk));
}
```

### Database

```typescript
import { sql, SQL } from "otter";

// SQLite
const db = new SQL(":memory:");
await db`CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)`;
await db`INSERT INTO users (name) VALUES (${"Alice"})`;
const users = await db`SELECT * FROM users`;

// PostgreSQL
const pg = new SQL("postgres://user:pass@localhost/db");

// COPY FROM for bulk import
await pg.copyFrom("users", {
  columns: ["name", "email"],
  format: "csv",
  source: new Blob(["Alice,alice@example.com\nBob,bob@example.com"]),
});

// KV store
import { kv } from "otter";
const store = kv(":memory:");
store.set("key", { data: "value" });
store.get("key");
```

### Module System

- ES Modules (import/export)
- CommonJS (require/module.exports)
- TypeScript (.ts, .tsx) without configuration
- Import maps
- `node:*` specifiers

## Platform Support

| Platform | Architecture  | JSC Source                     |
|----------|---------------|--------------------------------|
| macOS    | x86_64, ARM64 | bun-webkit (JIT) or system JSC |
| Linux    | x86_64, ARM64 | bun-webkit (static)            |
| Windows  | x86_64        | bun-webkit (static)            |

Set `OTTER_USE_SYSTEM_JSC=1` on macOS to use system JavaScriptCore (faster builds, no JIT).

## Performance

Measured on Apple M1:

| Metric      | Value |
|-------------|-------|
| Cold start  | ~30ms |
| Warm start  | ~10ms |
| Binary size | 38MB  |
| Standalone  | 38MB + user code |

PostgreSQL COPY FROM: 108K rows/sec (bulk import).

## Project Structure

```text
crates/
├── otter-jsc-sys      # JSC FFI bindings
├── otter-jsc-core     # Safe JSC wrappers
├── otter-runtime      # Event loop, extensions, Web APIs
├── otter-engine       # Module loader, capabilities
├── otter-node         # Node.js compatibility
├── otter-pm           # Package manager
├── otter-sql          # SQLite + PostgreSQL
├── otter-kv           # Key-value store
└── otterjs            # CLI binary
```

## Development

```bash
cargo build                              # debug build
cargo build --release -p otterjs         # release CLI
cargo test --all                         # run tests
cargo run -p otterjs -- run examples/basic.ts
```

## License

MIT
