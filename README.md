# Otter

![Otter Logo](./otter-logo.png)

Otter is a Rust-first JavaScriptCore runtime with an embeddable event loop, async ops, and a Bun/Deno-style CLI.

## Quick start

```bash
cargo run -p otter-cli -- run examples/basic.js
```

## CLI

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

## Notes

- The CLI wraps scripts in an async IIFE to support top-level async.
- The CLI waits for the event loop to become idle or until `--timeout-ms` is reached.
- `fetch` is Promise-based and exposes `Headers`, `Request`, `Response`, `Blob`, `FormData`, and `URLSearchParams`.
- Embedders can override console output via `set_console_handler`.

## Embedding

```rust
use otter_runtime::{ConsoleLevel, JscConfig, JscRuntime, set_console_handler};
use std::time::Duration;

set_console_handler(|level, message| match level {
    ConsoleLevel::Error | ConsoleLevel::Warn => eprintln!("{}", message),
    _ => println!("{}", message),
});

let runtime = JscRuntime::new(JscConfig::default())?;
runtime.eval("console.log('hello')")?;
runtime.run_event_loop_until_idle(Duration::from_millis(5000))?;
```
