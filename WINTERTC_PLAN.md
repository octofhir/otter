# WinterTC Minimum Common API Plan

Дата обновления: 2026-07-09.

## Цель

Довести активный runtime stack Otter до WinterTC / ECMA-429 Minimum Common
Web API 2025 snapshot без параллельных runtime stacks и без роста
`crates-legacy/*`.

Актуальный внешний источник истины: <https://min-common-api.proposal.wintertc.org/>
(draft от 28 April 2026).

## Scope

WinterTC требует:

- ECMA-262 conformance;
- Web platform globals on `globalThis`;
- Fetch classes and `fetch()`;
- DOM events / abort / message-channel basics;
- File, Blob, FormData;
- WHATWG Streams, including byte/BYOB pieces;
- Encoding, URL, URLPattern;
- WebCrypto globals and classes;
- Performance, timers, microtasks, `structuredClone`, `console`;
- WebAssembly JS/Web API namespace.

Web workers themselves are not required. If worker-like global scopes are
introduced later, their event-handler attributes must follow the same global
scope rules.

## Current Baseline

Relevant local state:

- `ES_CONFORMANCE.md`: 98.53% pass rate excluding skipped tests, captured
  2026-06-24.
- `crates/otter-cli/src/main.rs`: CLI builders already call
  `with_web_apis()`.
- `crates/otter-web`: active home for most Web API globals.
- `crates/otter-web/tests/web.rs`: has a WinterTC ledger, but it is
  incomplete against ECMA-429 and must be fixed before being used as the
  tracking source.

Currently implemented or partially implemented in `otter-web`:

- `URL`, `URLSearchParams`;
- `Blob`, JS `File`;
- `Event`, `EventTarget`, `CustomEvent`, `ErrorEvent`, `MessageEvent`,
  `MessageChannel`, `MessagePort`;
- `AbortController`, `AbortSignal`;
- `Headers`, `Request`, `Response`, `FormData`;
- `TextEncoder`, `TextDecoder`, `TextEncoderStream`, `TextDecoderStream`;
- default `ReadableStream`, `WritableStream`, `TransformStream`, default
  readers/writers/controllers, queuing strategies;
- `CompressionStream`, `DecompressionStream`;
- `crypto` object with `getRandomValues`, `randomUUID`, `subtle.digest`;
- `performance.now`, `performance.timeOrigin`;
- `navigator.userAgent`;
- `atob`, `btoa`, `queueMicrotask`, `structuredClone`;
- timers and `console` are installed elsewhere in the active runtime stack.

Important semantic gaps even where a global name exists:

- `fetch()` is a placeholder that throws "fetch is not implemented".
- `Blob.prototype.arrayBuffer()` currently returns text, not
  `Promise<ArrayBuffer>`.
- `URL` stores snapshot data properties instead of live spec accessors and
  does not expose the full URL interface.
- Streams are a practical default-stream implementation; byte streams and BYOB
  are absent.
- `crypto.subtle` supports only `digest`; WinterTC only names the classes and
  `crypto` global, but WebCrypto conformance needs an explicit algorithm
  matrix.
- `structuredClone` has ArrayBuffer transfer and in-realm cloning, but
  transferables such as `MessagePort` are future placeholders.

## Gap Ledger To Add

First fix `wintertc_minimum_common_api_ledger` so every ECMA-429 path appears
exactly once as `SUPPORTED`, `PARTIAL`, or `NOT_YET`.

Missing from the current ledger:

- `CustomEvent`, `ErrorEvent`, `MessageEvent`, `MessageChannel`,
  `MessagePort`, `PromiseRejectionEvent`;
- `globalThis.onerror`, `globalThis.onunhandledrejection`,
  `globalThis.onrejectionhandled`, `globalThis.reportError`,
  `globalThis.self`;
- `WebAssembly`, `WebAssembly.compile`, `compileStreaming`, `instantiate`,
  `instantiateStreaming`, `validate`, `JSTag`, and constructors/errors:
  `Global`, `Instance`, `Memory`, `Module`, `Table`, `Tag`, `Exception`,
  `CompileError`, `LinkError`, `RuntimeError`.

Known `NOT_YET` / missing API groups:

- `URLPattern`;
- `PromiseRejectionEvent`;
- `Crypto`, `CryptoKey`, `SubtleCrypto` constructor/class globals;
- `ReadableByteStreamController`, `ReadableStreamBYOBReader`,
  `ReadableStreamBYOBRequest`;
- `TransformStreamDefaultController`;
- `WebAssembly.*` namespace;
- global error / promise-rejection reporting surface if the Otter global object
  is treated as a Window/Worker-like EventTarget.

Known `PARTIAL` groups:

- `fetch`;
- `Blob` / `File`;
- `URL`;
- `Request` / `Response` body and streaming behavior;
- default Streams;
- WebCrypto;
- `structuredClone`;
- `MessagePort` if transfer semantics are required.

## Crate Boundaries

### `crates/otter-web`

Own all standard Web API JS-visible behavior:

- WebIDL-ish JS shims and native backing for Web classes.
- `URLPattern`.
- Fetch classes, body mixin, `fetch()` JS contract, and hidden plain-data
  integration hooks.
- `Blob`, `File`, `FormData`.
- DOM events, abort, message channel, `PromiseRejectionEvent`.
- Streams, byte streams, BYOB readers/requests/controllers.
- Encoding, compression streams, WebCrypto surface and native crypto backing.
- Performance, `navigator`, `self`, `reportError`, global event-handler
  properties when enabled.

Keep native backing data owned and sendable. Do not expose VM handles, `Rc`, or
`RefCell` across crate boundaries.

### `crates/otter-runtime`

Own runtime scheduling and host boundaries that Web APIs need:

- event-loop task dispatch and microtask checkpoints;
- promise rejection tracking hook data needed by `unhandledrejection` /
  `rejectionhandled`;
- async `fetch()` plumbing and cancellation/abort propagation;
- structured clone core for in-realm and future cross-isolate transfer;
- capability checks for outbound network fetch through `CapabilitySet`.

The runtime should expose plain DTO hooks to `otter-web`; it should not know
about HTTP status code rendering or server-specific APIs.

### `crates/otter-modules`

Own Otter-specific APIs and integrations:

- `Otter.serve` / `import { serve } from "otter"`;
- HTTP transport;
- conversion between HTTP request/response data and `otter-web` Fetch
  internals;
- net permission enforcement for server sockets.

Do not implement separate Request/Response/Header classes here.

### `crates/otter-vm`

Own engine-level language/runtime primitives:

- ECMA-262 features needed by Web APIs;
- WebAssembly JS/Web API namespace if added natively;
- generic host-object, promise, microtask, and handle-scope support.

Do not add Fetch, HTTP, URL, WebCrypto, or server-specific concepts to the VM.

### `crates/otter-cli`

Own default product wiring:

- enable `with_web_apis()` for CLI run/eval/dump paths;
- wire outbound fetch permission flags and default `User-Agent`;
- expose user-facing flags/docs only after the runtime surface is stable.

### `crates/otter-pm/src/types/otter`

Own TypeScript declarations for Otter-provided APIs. Standard Web API types
should be aligned with the runtime install surface; generated publish artifacts
under `packages/otter-types/` should not be edited directly.

## Implementation Slices

1. **Authoritative WinterTC ledger**
   - Replace the two-bucket test with `SUPPORTED` / `PARTIAL` / `NOT_YET`.
   - Include every ECMA-429 path, including `WebAssembly.*`.
   - Add a second test that asserts `PARTIAL` entries have executable smoke
     checks documenting the known limitation.

2. **Global scope shell**
   - Add `self === globalThis`.
   - Add `reportError`.
   - Decide and document whether Otter's global object maps to Window-like /
     Worker-like EventTarget. If yes, add `onerror`,
     `onunhandledrejection`, `onrejectionhandled`, and event dispatch. If no,
     document the alternative reporting mechanism and omit those properties
     per ECMA-429 section 6.

3. **Fetch v1**
   - Move `fetch()` from placeholder to async host-backed operation.
   - Enforce `allow-net` deny-by-default in `otter-runtime`/CLI capability
     boundary.
   - Return real `Response` objects from `otter-web` internals.
   - Support `AbortSignal`, redirects policy, default `User-Agent`, headers,
     buffered request/response bodies first.
   - Later slice: streamed request/response bodies with backpressure.

4. **Blob/File correctness**
   - Fix `Blob` constructor to accept iterable blob parts, BufferSource, Blob,
     and strings.
   - Make `arrayBuffer()` return `Promise<ArrayBuffer>`.
   - Add `bytes()` if tracking the current File API living standard surface,
     but keep ECMA-429 conformance focused on the required subset.
   - Keep `File` metadata and Blob slicing behavior spec-shaped.

5. **URL and URLPattern**
   - Replace snapshot `URL` data properties with accessors backed by `WebUrl`,
     including setters where the URL Standard requires them.
   - Expose `searchParams` and keep it live with `URL.search`.
   - Add `URL.canParse` / `URL.parse` only if required by the referenced URL
     Standard snapshot used by tests.
   - Add `URLPattern` in `otter-web`, preferably through a proven URLPattern
     parser crate or a small spec-focused parser with dedicated tests.

6. **Streams byte/BYOB and Transform controller**
   - Add `ReadableByteStreamController`.
   - Add `ReadableStreamBYOBReader` and `ReadableStreamBYOBRequest`.
   - Add `TransformStreamDefaultController` global and prototype behavior.
   - Rework `Response.body` / `Request.body` to consume these semantics.

7. **WebCrypto class globals**
   - Expose `Crypto`, `SubtleCrypto`, `CryptoKey` constructors/classes with
     correct branding.
   - Keep `crypto` singleton backed by `Crypto.prototype`.
   - Keep `digest` as the first green algorithm set; add an explicit algorithm
     support matrix before adding keys/sign/encrypt/import/export.

8. **Promise rejection and error events**
   - Add `PromiseRejectionEvent`.
   - Wire VM/runtime promise rejection tracking to global event dispatch or
     documented alternative.
   - Ensure `reportError()` and uncaught exception reporting carry
     `ErrorEvent` information where applicable.

9. **WebAssembly namespace**
   - Add `WebAssembly` in `otter-vm` or a focused support crate if the
     implementation becomes large.
   - Minimum shape: constructors/errors/statics required by ECMA-429.
   - Decide whether v1 is a real wasm execution backend or a documented
     non-conformant placeholder; do not mark supported until validate /
     compile / instantiate behavior is real enough for conformance tests.

10. **Types, docs, and examples**
    - Update Otter `.d.ts` sources for every added global or Otter-specific
      fetch/serve capability flag.
    - Add docs under `docs/site/src/content/docs/` for contributor-facing Web
      API boundaries once behavior lands.
    - Keep `SERVE_PLAN.md` aligned when Fetch/Streams changes affect
      `Otter.serve`.

## Validation Loop

Per slice:

```bash
cargo test -p otter-web
cargo test -p otter-runtime
cargo test -p otter-cli
```

For ECMA-262-sensitive support work:

```bash
just test262-filter "<affected built-ins path>"
```

For Web API behavior, add focused tests under `crates/otter-web/tests/web.rs`
until a dedicated Web Platform Tests runner exists.

Recommended smoke matrix:

- `typeof globalThis[name] !== "undefined"` for every ECMA-429 path;
- constructor/prototype branding and `[Symbol.toStringTag]`;
- property descriptors for globals;
- body read methods return promises and respect `bodyUsed`;
- abort/cancel paths do not leak runtime keep-alives;
- `OTTER_GC_STRESS=1..16 cargo test -p otter-web` for native-backed objects.
