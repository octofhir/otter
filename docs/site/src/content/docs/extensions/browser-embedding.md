---
title: "Browser Embedding"
---

Otter's browser-oriented embedding shape is one shared host and one isolate per
document or worker. The host is process-wide infrastructure; it is not a
JavaScript realm.

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
    .build_handle()?;
let page_b = Runtime::builder()
    .runtime_host(host.clone())
    .build_handle()?;
```

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

Install `RuntimeBuilder::promise_rejection_hook` per page to translate the
isolate's unhandled/later-handled rejection checkpoints into the browser's
`unhandledrejection` and `rejectionhandled` event path. The Rust hook receives
the live promise and reason on the owning thread; it replaces the legacy magic
JavaScript reporter global.

Execute an already-fetched module entry with
`Runtime::run_module_source(source, canonical_url)`. The entry stays in memory;
static dependencies resolve relative to its URL and use the configured loader
or remote-fetch cache. File-backed `Runtime::run_module(path)` remains the CLI
path and shares the same linker/evaluator after loading.

## Standalone CLI

The CLI uses the same runtime builders and Tokio host with one isolate. Browser
storage, DOM bindings, and the browser event hub remain product extensions, so
they do not enter the CLI dependency graph.
