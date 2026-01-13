# Otter Architecture

## Overview

Otter is a Rust-based JavaScriptCore runtime designed for embedding in high-load servers. It provides a safe, async API for executing TypeScript/JavaScript code with host-provided extensions.

## Crate Structure

```
┌────────────────────────────────────────────────────────────────┐
│                      otter-cli                                  │
│  CLI commands, permission parsing, config loading              │
└────────────────────────────┬───────────────────────────────────┘
                             │
┌────────────────────────────▼───────────────────────────────────┐
│                      otter-node                                 │
│  Node.js API implementations: fs, path, buffer, url            │
└────────────────────────────┬───────────────────────────────────┘
                             │
┌────────────────────────────▼───────────────────────────────────┐
│                     otter-engine                                │
│  EngineHandle, ESM loader, module graph, capabilities          │
└────────────────────────────┬───────────────────────────────────┘
                             │
┌────────────────────────────▼───────────────────────────────────┐
│                    otter-runtime                                │
│  Event loop, timers, extensions, Promise driver, transpiler    │
└────────────────────────────┬───────────────────────────────────┘
                             │
┌────────────────────────────▼───────────────────────────────────┐
│                      jsc-core                                   │
│  Safe wrappers: Context, Value, Object, Function, Exception    │
│  Thread safety markers (!Send, !Sync), RAII patterns           │
└────────────────────────────┬───────────────────────────────────┘
                             │
┌────────────────────────────▼───────────────────────────────────┐
│                       jsc-sys                                   │
│  Raw FFI only, all unsafe, minimal dependencies                │
│  Platform-conditional: macOS framework / Linux GTK             │
└────────────────────────────────────────────────────────────────┘
```

### jsc-sys

Raw FFI bindings to JavaScriptCore C API.

**Responsibilities:**
- Type definitions: JSContextRef, JSValueRef, JSObjectRef, etc.
- Extern function declarations
- Platform-specific linking (macOS framework, Linux pkg-config)
- Constants: property attributes, type enums

**Constraints:**
- ALL code is `unsafe`
- NO safe wrappers
- NO business logic
- Minimal dependencies (only libc)

### jsc-core

Safe Rust wrappers around JSC primitives.

**Responsibilities:**
- `JscContext`: Global context management, eval, GC
- `JscValue`: Value with automatic GC protection/unprotection
- `JscObject`: Object property access
- `JscFunction`: Function creation and calling
- `JscException`: Structured error extraction
- Thread safety markers: `!Send`, `!Sync` on context types

**Key Types:**
```rust
pub struct JscContext {
    ctx: jsc_sys::JSGlobalContextRef,
    _not_send: PhantomData<*mut ()>,
}

// Compiler enforces single-thread usage
impl !Send for JscContext {}
impl !Sync for JscContext {}

pub struct JscValue {
    value: jsc_sys::JSValueRef,
    ctx: jsc_sys::JSContextRef,
}

// Values also bound to their context's thread
impl !Send for JscValue {}
impl !Sync for JscValue {}
```

### otter-runtime

Core runtime functionality.

**Responsibilities:**
- Event loop (timers, microtasks)
- Extension system (sync/async ops)
- Promise driver (background polling)
- TypeScript transpilation (SWC)
- Native APIs (console, fetch, timers)

### otter-engine

High-level embedding API.

**Responsibilities:**
- `Engine`: Manages worker threads and contexts
- `EngineHandle`: Thread-safe handle for job submission
- ESM module loader
- Module graph with cycle detection
- Capability-based security

### otter-node

Node.js compatibility layer.

**Responsibilities:**
- `node:fs` implementation
- `node:path` implementation
- `node:buffer` implementation
- `node:url` implementation
- `node:process` (minimal)

### otter-cli

Command-line interface.

**Responsibilities:**
- `otter run` - execute scripts
- `otter check` - type check with tsgo
- `otter test` - run tests
- `otter repl` - interactive mode
- Permission flag parsing
- Config file loading

## Threading Model

### Pool of Dedicated Runtime Threads

Optimized for high-load FHIR server with event-driven TypeScript automation:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Server Threads                                │
│  (request handlers, event processors hold EngineHandle)         │
└───────────────┬─────────────────────────────────────────────────┘
                │ submit(job) - sends via crossbeam mpmc channel
                ▼
┌─────────────────────────────────────────────────────────────────┐
│         Shared Job Queue (crossbeam mpmc channel)                │
│  Jobs: Box<dyn FnOnce(&mut JscContext) -> Result<Value> + Send> │
│  - Work-stealing friendly: any thread can pick up any job       │
│  - Backpressure: bounded queue with try_send/timeout options    │
└──────┬────────────────┬────────────────┬───────────────────────┘
       │                │                │
       ▼                ▼                ▼
┌──────────────┐ ┌──────────────┐ ┌──────────────┐
│ Runtime      │ │ Runtime      │ │ Runtime      │
│ Thread #0    │ │ Thread #1    │ │ Thread #N    │
│              │ │              │ │              │
│ JscContext   │ │ JscContext   │ │ JscContext   │
│ (!Send)      │ │ (!Send)      │ │ (!Send)      │
│              │ │              │ │              │
│ EventLoop    │ │ EventLoop    │ │ EventLoop    │
│ Extensions   │ │ Extensions   │ │ Extensions   │
│              │ │              │ │              │
│ Loop:        │ │ Loop:        │ │ Loop:        │
│ 1.recv job   │ │ 1.recv job   │ │ 1.recv job   │
│ 2.execute    │ │ 2.execute    │ │ 2.execute    │
│ 3.poll evts  │ │ 3.poll evts  │ │ 3.poll evts  │
│ 4.respond    │ │ 4.respond    │ │ 4.respond    │
└──────────────┘ └──────────────┘ └──────────────┘
```

### Key Invariants

1. `JscContext`, `JscValue`, `JSValueRef` are `!Send + !Sync`
2. Only the owning runtime thread touches JSC objects
3. `EngineHandle` is `Send + Sync + Clone` - safe to share across threads
4. Job results returned via oneshot channels

### Cross-Thread API

```rust
// Thread-safe handle - can be cloned and shared
pub struct EngineHandle {
    job_tx: crossbeam_channel::Sender<Job>,
}

unsafe impl Send for EngineHandle {}
unsafe impl Sync for EngineHandle {}

impl EngineHandle {
    pub async fn eval(&self, script: String) -> JscResult<serde_json::Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.job_tx.send(Job::Eval { script, response: tx })?;
        rx.await.map_err(|_| JscError::PoolExhausted)?
    }

    pub async fn eval_typescript(&self, code: String) -> JscResult<serde_json::Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.job_tx.send(Job::EvalTypeScript { code, response: tx })?;
        rx.await.map_err(|_| JscError::PoolExhausted)?
    }
}
```

## TypeScript Architecture

### Pipeline

```
┌───────────────┐    ┌────────────────┐    ┌──────────────┐
│  TypeScript   │───▶│   Type Check   │───▶│  Transpile   │
│  Source File  │    │   (tsgo)       │    │   (SWC)      │
└───────────────┘    └────────────────┘    └──────────────┘
                            │                     │
                            ▼                     ▼
                     ┌────────────┐        ┌───────────┐
                     │ Diagnostics│        │ JavaScript│
                     │ (errors)   │        │ + SourceMap│
                     └────────────┘        └───────────┘
                                                 │
                                                 ▼
                                          ┌───────────┐
                                          │    JSC    │
                                          │  Execute  │
                                          └───────────┘
```

### tsgo Integration

tsgo (TypeScript Go compiler) is always used for type checking - 10x faster than tsc.

```rust
pub struct TypeCheckConfig {
    pub enabled: bool,
    pub tsconfig_path: Option<PathBuf>,
    pub strict: bool,
    pub skip_lib_check: bool,
}

pub async fn check_types(
    files: &[PathBuf],
    config: &TypeCheckConfig,
) -> Result<Vec<Diagnostic>, TypeCheckError> {
    if !config.enabled {
        return Ok(vec![]);
    }

    let tsgo_path = which::which("tsgo")
        .map_err(|_| TypeCheckError::TsgoNotFound)?;

    let mut cmd = tokio::process::Command::new(tsgo_path);
    cmd.arg("--noEmit");
    // ... configure and run
}
```

### Built-in Type Definitions

```
crates/otter-runtime/types/
├── lib.esnext.d.ts           # ES2024+ builtins
├── lib.dom.d.ts              # DOM APIs (subset)
├── node/                     # @types/node bundled
│   ├── fs.d.ts
│   ├── path.d.ts
│   ├── buffer.d.ts
│   └── ...
└── otter.d.ts                # Otter-specific globals
```

## Extension System

### Host Registration

```rust
let extension = Extension::new("fhir")
    .op_sync("getResource", |ctx, args| {
        let resource_type = args[0].as_str().unwrap();
        let id = args[1].as_str().unwrap();
        // ... fetch resource
        Ok(json!(resource))
    })
    .op_async("search", |ctx, args| async move {
        let query = args[0].clone();
        // ... execute search
        Ok(json!(results))
    })
    .with_state::<DbPool>(db_pool);

engine.register_extension(extension);
```

### JavaScript Access

```javascript
// Sync call
const patient = fhir.getResource("Patient", "123");

// Async call returns Promise
const results = await fhir.search({ resourceType: "Patient", name: "Smith" });
```

### Type Conversion Rules

| Rust Type | JS Type | Notes |
|-----------|---------|-------|
| `()`, `None` | `undefined` | |
| `bool` | `boolean` | |
| `i32`, `i64`, `f64` | `number` | i64 may lose precision |
| `String`, `&str` | `string` | UTF-8 encoded |
| `Vec<T>` | `Array` | Recursive conversion |
| `HashMap<String, T>` | `Object` | Keys must be strings |
| `serde_json::Value` | Any | Pass-through |

## Security Model

### Capability-Based Permissions

```rust
pub struct Capabilities {
    pub fs_read: Option<Vec<PathBuf>>,   // None = denied
    pub fs_write: Option<Vec<PathBuf>>,
    pub net: Option<Vec<String>>,         // Host allowlist
    pub env: Option<Vec<String>>,         // Env var allowlist
    pub subprocess: bool,
    pub ffi: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            fs_read: None,      // Denied by default
            fs_write: None,
            net: None,
            env: None,
            subprocess: false,
            ffi: false,
        }
    }
}
```

### Resource Limits

```rust
pub struct ResourceLimits {
    pub max_heap_mb: Option<u32>,         // Soft limit via periodic GC
    pub max_eval_time: Option<Duration>,  // Per-eval timeout
    pub max_recursion: Option<u32>,       // Stack depth
}
```

## Observability

### Tracing Integration

```rust
#[instrument(skip(ctx), fields(source_url))]
pub fn eval(&self, script: &str, source_url: &str) -> Result<JscValue> {
    // Span captures timing, errors
}
```

### Debug Hooks

```rust
pub trait DebugHook: Send + Sync {
    fn on_script_start(&self, source: &str);
    fn on_script_end(&self, result: &Result<JscValue>);
    fn on_exception(&self, error: &JscError);
}
```

## ESM Module Loading

### Resolution Order

1. `file://` - Local filesystem
2. `node:` - Built-in Node.js modules
3. `https://` - Remote modules (with whitelist)

### Whitelist Configuration

```toml
[modules]
remote_allowlist = [
  "https://esm.sh/*",
  "https://cdn.skypack.dev/*",
  "https://unpkg.com/*",
]
remote_cache_dir = ".otter/cache/modules"
```

### Module Graph

- Cycle detection to prevent infinite loops
- Lazy loading for performance
- Source map support for debugging
