---
title: "Browser Embedding"
---

Otter's browser-oriented embedding shape is one shared host and one isolate per
document or worker. The host is process-wide infrastructure; it is not a
JavaScript realm.

Use `otter_runtime::embedding` as the application-facing import surface. It is
the curated, owned API and intentionally excludes interpreter, value, object,
GC-handle, and native mutation types.

```text
BrowserHost
  ├─ TokioRuntimeHost (executor, timers, HTTP client)
  ├─ origin storage + broadcast registries       [browser-owned]
  ├─ page A → Runtime / RuntimeHandle             [own heap + realm]
  └─ page B → Runtime / RuntimeHandle             [own heap + realm]
```

Create one ready-made Tokio host and clone it into each runtime builder:

```rust,ignore
let host = TokioRuntimeHost::new()?;

let page_a = Runtime::builder()
    .runtime_host(host.clone())
    .build_handle_async().await?;
let page_b = Runtime::builder()
    .runtime_host(host.clone())
    .build_handle_async().await?;
```

The synchronous `build_handle()` remains available for CLI/tests. Browser page
creation should use `build_handle_async()` (or `OtterBuilder::build_async()`),
which runs isolate bootstrap and its startup handshake on Tokio's blocking pool
instead of parking the UI/async caller.

The isolates share executor-backed host services, but never `globalThis`, a GC
heap, roots, microtasks, capability state, or JavaScript values. A direct
single-threaded `Runtime` can use the same host's Tokio handle for owned async
work while DOM access remains on the browser's main thread.

## Cross-page events

`localStorage`, the `storage` event, `BroadcastChannel`, browsing-context
groups, and origin partitioning are browser semantics and therefore belong in
the browser host, not in the JavaScript engine. Store only owned Rust data in
that host. To notify a Layer B page, enqueue a `RuntimeTask` through its
`RuntimeHandle`/`RuntimeTaskSpawner`; the task materializes the JavaScript Event
inside the target isolate. For Layer A, post the owned message into the
browser's event queue and materialize it with `Runtime::run_native_event` on the
runtime's owning thread.

Never send `Value`, `JsObject`, DOM arena references, `NativeCtx`, or GC roots
between pages. A later process split should only change the transport for the
same owned message DTOs.

Keep the `RuntimeExecutionContext` returned by
`Runtime::run_script_with_context` with the page. Each platform, storage, or
broadcast delivery is one call to `Runtime::run_native_event(&context, ...)`;
nested JavaScript dispatch remains synchronous and the method performs the
single microtask checkpoint at the end of the outer host task.

When a page closes, call `RuntimeHandle::shutdown` before removing it from the
browser registry. Every clone observes the shutdown state and late task
delivery fails. For async results that must become rich JavaScript objects,
deliver an owned result DTO as a runtime task and call
`Runtime::settle_pending_promise_with` inside that task; its materializer runs
in a handle scope on the target isolate.

Layer B `RuntimeHandle::settle_promise` is also non-blocking under inbox
pressure: it retains the owned result and retries delivery on the shared Tokio
host. Shutdown cancels the retry and releases its liveness accounting.

Install `RuntimeBuilder::promise_rejection_hook` per page to translate the
isolate's unhandled/later-handled rejection checkpoints into the browser's
`unhandledrejection` and `rejectionhandled` event path. The Rust hook receives
the live promise and reason on the owning thread; it replaces the legacy magic
JavaScript reporter global.

Execute an already-fetched module entry with
`RuntimeHandle::run_module_source(source, canonical_url)`. The entry stays in
memory. Install an embedder transport with
`RuntimeBuilder::remote_module_provider`; its `RemoteModuleProvider::fetch`
method receives an owned URL and `ModuleLoadCancellation`, and returns an owned
future. Otter capability-checks each target, fetches at most eight remote graph
nodes concurrently, caches requested and post-redirect canonical URLs, and
moves AST/compile/link work to Tokio's blocking pool. Static graphs and dynamic
imports use this same pipeline. Command timeout, dropped waiters, and
`RuntimeHandle::shutdown` cancel in-flight provider work. Browser origin and
CORS policy remain browser-owned and may be enforced by the provider.

File-backed `RuntimeHandle::run_module(path)` shares that graph pipeline.
Direct thread-pinned `Runtime` remains network-transport agnostic; use the
sendable handle when a graph can require asynchronous remote I/O.

`RuntimeBuilder::timeout` is also enforced for direct `Runtime::run_*` calls.
The watchdog interrupts interpreter loops and graph preparation, reports
`OtterError::Timeout`, clears the interrupt, and leaves the runtime reusable.
`Duration::ZERO` disables this deadline.

Additional same-agent globals use opaque realms. Both classic scripts and
module graphs have direct and sendable high-level entry points:

```rust,ignore
let frame = page.create_realm().await?;
page.run_script_in_realm(frame, classic_source, "frame:inline-1").await?;
page.run_module_source_in_realm(
    frame,
    module_source,
    "https://example.test/frame/main.js",
).await?;
page.dispose_realm(frame).await?;
```

Each realm owns its global lexicals, module environments, module evaluation
records, dynamic imports, host-promise settlement target, and timers. Its
canonical-URL module map persists across separate entry graphs, so shared
dependencies execute once per realm. Disposal cancels timers and rejects late
realm routing by invalidating the opaque id. The isolate/agent still owns one
FIFO task and microtask boundary shared by its realms.

## Standalone CLI

The CLI uses the same runtime builders and Tokio host with one isolate. Browser
storage, DOM bindings, and the browser event hub remain product extensions, so
they do not enter the CLI dependency graph.
