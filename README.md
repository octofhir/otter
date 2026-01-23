# Otter

TypeScript/JavaScript runtime with a custom bytecode VM.

## Overview

Otter is a TypeScript/JavaScript runtime that can be used as:

1. **Embeddable engine** - Add scripting to Rust applications via `otter-vm-runtime` crate
2. **Standalone CLI** - Run scripts directly with `otter run script.ts`

Built on a custom bytecode VM with garbage collection, written entirely in Rust.

> **Note:** The VM is currently under active development. Some features are temporarily disabled.

## Installation

### As Rust Library

```toml
[dependencies]
otter-vm-runtime = "0.1"
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
use otter_vm_runtime::Runtime;

fn main() -> anyhow::Result<()> {
    let mut runtime = Runtime::new();

    runtime.eval(r#"
        const user = { name: "Alice", age: 30 };
        console.log(JSON.stringify(user));
    "#)?;

    Ok(())
}
```

> **Note:** TypeScript support and full API surface are being ported to the new VM.

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
| `async_hooks` | ✅ Full | 100% - AsyncResource + AsyncLocalStorage keep stores/callbacks across timers & microtasks |
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
| `util` | ✅ Full | 100% - promisify, inspect, format, formatWithOptions, debuglog, parseArgs, MIMEType, types, callbackify, isDeepStrictEqual |
| `worker_threads` | ✅ Full | 100% - Real threading: Worker, MessageChannel, MessagePort, BroadcastChannel, workerData |
| `zlib` | ✅ Full | 100% - gzip/deflate/brotli with chunkSize/dictionary + CRC32 + stream classes |

**Not yet implemented:** `cluster`, `tls`, `vm`, `perf_hooks`, `inspector`

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

| Platform | Architecture  |
|----------|---------------|
| macOS    | x86_64, ARM64 |
| Linux    | x86_64, ARM64 |
| Windows  | x86_64        |

Pure Rust implementation - no external JavaScript engine dependencies.

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
├── otter-vm-bytecode  # Bytecode definitions
├── otter-vm-gc        # Garbage collector
├── otter-vm-core      # VM interpreter
├── otter-vm-compiler  # JS/TS to bytecode compiler
├── otter-vm-runtime   # Runtime with builtins
├── otter-vm-builtins  # Built-in functions
├── otter-engine       # Module loader, capabilities
├── otter-node         # Node.js compatibility (reference)
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
