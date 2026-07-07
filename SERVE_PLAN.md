# Otter.serve Plan

Дата обновления: 2026-07-07.

## Цель

Сделать `Otter.serve` пригодным для SSR/Hono/Vite-style workloads:

- быстрый HTTP server без блокировки VM/event loop;
- стандартный Fetch contract: `Request`, `Response`, `Headers`, Web Streams;
- удобная surface area для контрибьюторов без hidden hacks и параллельных
  runtime stacks;
- общий event-loop/liveness primitive для будущих `node:net`, fetch, watchers,
  WebSocket и других long-lived host resources.

## Принятые Решения

1. `serve` живет в `crates/otter-modules`, потому что это Otter-specific API.
2. `otter-web` владеет Web APIs: `Request`, `Response`, `Headers`,
   `ReadableStream`, body mixin и Fetch internals.
3. `otter-runtime` владеет event-loop liveness, isolate inbox и dispatch owned
   runtime tasks.
4. `otter-vm` не знает про HTTP, status codes, server objects, Fetch classes
   или transport DTO.
5. Public API:
   - `Otter.serve(options)`
   - `import { serve } from "otter"`
6. Server lifecycle:
   - `server.stop()` / `server.close()`
   - `server.ref()`
   - `server.unref()`
   - `server.url`, `server.hostname`, `server.port`
7. v1 transport scope: HTTP/1.1 first. TLS, WebSocket, HTTP/2/H3, Unix sockets
   and route-level optimizations are later slices.
8. Bodies must move toward Web Streams. Fully buffered bytes are acceptable only
   as a bootstrap path while request/response streaming and backpressure land.

## Boundaries

- `otter-modules::serve`
  - parse options;
  - enforce net permissions;
  - own HTTP transport;
  - convert HTTP request/response data to/from Fetch internals;
  - own `Server` host object.
- `otter-web`
  - own standard Fetch/Web Streams JS-visible behavior;
  - expose hidden plain-data internals for server integration;
  - keep `Request.body` / `Response.body` semantics compatible with Web APIs.
- `otter-runtime`
  - own `RuntimeKeepAlive`;
  - own typed `RuntimeTask` dispatch to the isolate thread;
  - keep the event loop alive while referenced host resources exist.
- `otter-vm`
  - provide generic persistent roots/native context helpers only;
  - no server-specific hooks.

## Current Status

- `Otter.serve` and bare module `"otter"` are wired.
- Server returns immediately and keeps the runtime alive through
  `RuntimeKeepAlive`.
- Requests enter the isolate through typed `RuntimeTask` dispatch, not polling.
- Server callback roots are stored through generic VM persistent roots.
- Status text comes from `http::StatusCode::canonical_reason()`.
- Request body has a `ServeBody` boundary and enters JS as bytes; `Request.body`
  exposes the existing one-chunk Web `ReadableStream` layer.
- Current transport is still a bootstrap `TcpListener` path with
  `Connection: close`; this is not the final high-performance backend.

## Next Steps

1. **Async fetch dispatch**
   - Allow `fetch(request)` to return `Response | Promise<Response>`.
   - Await promise settlement on the isolate event loop.
   - Required before handlers can `await request.text()`, `request.json()`, or
     stream reads.

2. **Request body streaming**
   - Replace full request buffering with host-backed
     `ReadableStream<Uint8Array>`.
   - Preserve deny-by-default capability boundaries.
   - Add abort/timeout cleanup for in-flight request bodies.

3. **Response body streaming**
   - Support `Response.body` as `ReadableStream`.
   - Stream chunks to the HTTP writer with backpressure.
   - Keep buffered string/bytes fast paths.

4. **Transport backend**
   - Replace bootstrap `TcpListener` handling with a proper async HTTP backend.
   - Keep VM interaction on the isolate thread only.
   - Reuse the same runtime task/liveness primitives for future `node:net`.

5. **Hono blockers**
   - Fix async-return thenable adoption in async functions.
   - Support update expressions on member/private-field operands
     (`++obj.x`, `++this.#field`, `obj.x--`).
   - Ensure `.ts` module detection works for package entrypoints.
   - Fill URL/searchParams/prototype gaps if Hono hits them.

6. **Types and docs**
   - Update real Otter `.d.ts` source for `serve`.
   - Remove or regenerate stale publish artifacts.
   - Document contributor-facing server/runtime boundaries.

7. **Benchmarks**
   - Add Hono end-to-end smoke.
   - Add throughput and latency benchmarks against Node and other runtimes using
     the same app.
   - Track cold start separately from steady-state throughput.

## Validation Loop

Run after each server/runtime slice:

```bash
cargo test -p otter-modules
cargo test -p otter-runtime runtime_keep_alive_liveness_is_idempotent
cargo test -p otter-runtime runtime_task_runs_on_isolate_loop
```

Smoke checks:

- `Otter.serve` global exists.
- `import { serve } from "otter"` works.
- HTTP request returns `Response`.
- POST request exposes `request.body instanceof ReadableStream`.
- `server.stop()`, `server.ref()`, and `server.unref()` keep correct liveness.
