# Otter Roadmap — the best JS runtime in the world

North star: a JavaScript runtime that is (a) more spec-correct than Node/Bun/Deno, (b) faster than Bun on steady-state and faster than Node on cold-start, (c) the best developer experience in the space, and (d) the only runtime that treats capability-based security, native FFI, and FHIR-native healthcare tooling as first-class surfaces. Nothing less is worth shipping.

This is the forward plan. The v2 migration (M0–M36) closed the whole JS syntax surface on the new stack; this document picks up from there.

## Quality gate (every milestone)

```
timeout 180 cargo build --workspace
timeout 90  cargo clippy --workspace --all-targets -- -D warnings
timeout 30  cargo fmt --all --check
timeout 180 cargo test --workspace
```

Plus: each track declares its own numeric acceptance criterion (pass rate, ns/op, RSS, cold-start ms). No milestone lands without the criterion met.

## Tracks

Work proceeds on eleven parallel tracks. Tracks share the same VM but can usually ship independently. Each milestone is one commit (`feat(scope): … (Txx)`) plus a `docs(roadmap): record Txx commit hash` follow-up, same pattern as the v2 tracker.

- **E — ECMAScript conformance** — `test262` pass rate to 95%+
- **P — Performance** — interp + JIT on par with V8 baseline, ahead of Bun on allocation-heavy code
- **G — GC & memory** — generational, incremental, sub-10ms pauses at 1GB heap
- **J — JIT compiler** — optimising tier + deopt + speculative inlining
- **W — Web APIs** — fetch, streams, WebCrypto, URL, WebSocket, Workers, WebAssembly host
- **N — Node.js compatibility** — `npm install` + run top 1000 packages
- **D — Developer experience** — CDP debugger, source maps, REPL, profiler, diagnostics
- **T — Tooling** — package manager, bundler, test runner, formatter, linter
- **S — Security** — capability audit, signed packages, per-module permissions
- **F — FFI & native interop** — C ABI, NAPI, Rust-callable, `dlopen` on capability
- **X — Differentiators** — FHIR-native module, capabilities for LLM sandboxes, multi-tenant isolates

---

## Track E — ECMAScript conformance

**North star:** 95%+ on `test262` across all language suites by milestone E10. Every regression gate keeps the pass rate at-or-above its previous milestone.

| ID  | Scope                                                                                            | Status | Commit |
|-----|--------------------------------------------------------------------------------------------------|--------|--------|
| E1  | BigInt primitive auto-boxing for method calls (`5n.toString()` → wraps + calls prototype method) | [x]    | cf99a87 |
| E2  | Full Proxy/Reflect surface: `apply`, `construct`, `getPrototypeOf`, `setPrototypeOf`, etc.       | [ ]    |        |
| E3  | WeakRef + FinalizationRegistry conformance (spec-correct liveness observer timing)               | [ ]    |        |
| E4  | Temporal API (Stage-3 proposal) behind `--harmony-temporal`                                      | [ ]    |        |
| E5  | Intl (ECMA-402): `Intl.Collator`, `DateTimeFormat`, `NumberFormat`, `PluralRules`                | [ ]    |        |
| E6  | ShadowRealm (Stage-3 proposal) — isolated evaluation contexts                                    | [ ]    |        |
| E7  | Iterator helpers (`.map`, `.filter`, `.take`, `.flatMap` on iterator protocol)                   | [ ]    |        |
| E8  | Atomics + SharedArrayBuffer full conformance; `Atomics.waitAsync`                                | [ ]    |        |
| E9  | Error.cause + AggregateError + structuredClone (HTML standard, spec-compliant)                   | [ ]    |        |
| E10 | Test262 conformance pass: target **≥95% language, ≥90% built-ins**                               | [ ]    |        |

Acceptance per milestone: `just test262` pass-rate delta > 0 on the relevant sub-tree, no regressions elsewhere.

---

## Track P — Performance

**North star:** beat Bun on allocation-heavy synthetic benchmarks; within 20% of V8 on sunspider/octane. Steady-state interp ≥ 300 Mops/s on int32 tight loops; JIT ≥ 1.5 Gops/s.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| P1  | Inline-cached property access (polymorphic IC: 4 shapes, fallback to megamorphic probe)             | [ ]    |        |
| P2  | Dense array fast-path: `push`/`pop`/`shift`/`unshift`/`slice`/`concat` skip hidden-class transitions | [ ]    |        |
| P3  | String ropes: `+` on strings > 128 chars builds lazy concat tree; flatten on `.length`/compare      | [ ]    |        |
| P4  | Object literal shape prediction: `{a:1, b:2}` → pre-shaped emit, not three transitions              | [ ]    |        |
| P5  | Fast-path integer `toString(10)` (no allocation for < 10⁸)                                          | [ ]    |        |
| P6  | Argument-adapter frame elimination when arity matches (no `arguments` object materialisation)        | [ ]    |        |
| P7  | Hidden-class sharing across isolates (global shape cache)                                           | [ ]    |        |
| P8  | SIMD-accelerated string scanning (ASCII-only hot paths: `indexOf`, `split`, `includes`)             | [ ]    |        |
| P9  | Branch prediction feedback → JIT layout (hot branch first)                                          | [ ]    |        |
| P10 | Startup time: cold-start < 20 ms for `otterjs run hello.js` on M-series; < 40 ms on x86_64          | [ ]    |        |

Each milestone ships microbenchmarks + criterion results vs `bun run` / `node`.

---

## Track G — GC & memory

**North star:** sub-10 ms GC pause at 1 GB live heap; mutator overhead < 5% vs no-GC. Deterministic at-rest RSS.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| G1  | Generational GC: young gen nursery (2 MB), bump-pointer alloc, copy collector with remembered set  | [ ]    |        |
| G2  | Incremental marking for old gen (tri-colour, write barrier, 5 ms slice budget)                     | [ ]    |        |
| G3  | Parallel mark (2+ worker threads per isolate)                                                      | [ ]    |        |
| G4  | Compact on fragmentation: old gen slab compaction behind a budget                                  | [ ]    |        |
| G5  | Weak-ref + finalization callbacks fire on a microtask (spec-compliant timing)                       | [ ]    |        |
| G6  | Per-isolate heap cap enforcement (catchable RangeError, like `--max-heap-bytes` today)              | [ ]    |        |
| G7  | Heap snapshot format compatible with Chrome DevTools (`.heapsnapshot`)                             | [ ]    |        |
| G8  | Memory profiling: allocation sampling + retention analysis (feeds into D5 profiler UI)              | [ ]    |        |
| G9  | Off-heap storage for ArrayBuffer backing stores (mmap + lazy-commit)                               | [ ]    |        |
| G10 | Escape analysis → stack-allocated short-lived objects                                              | [ ]    |        |

---

## Track J — JIT compiler

**North star:** three-tier JIT (interp → baseline → optimising), spec-compliant deoptimisation, inline caches, type-feedback-driven speculative specialisation.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| J1  | Optimising tier on top of baseline (Sea-of-Nodes or Turboshaft-style IR)                           | [ ]    |        |
| J2  | Type feedback propagation + speculative specialisation (monomorphic int32 in JIT body)             | [ ]    |        |
| J3  | Deoptimisation: bailout to interpreter at checkpoint, register-map encoded per safepoint           | [ ]    |        |
| J4  | Inline caching in JIT code (load-pc-relative guard → slow path)                                    | [ ]    |        |
| J5  | On-stack replacement (OSR) from optimising tier back to interpreter                                | [ ]    |        |
| J6  | Register allocation: linear-scan with hot-loop pinning                                              | [ ]    |        |
| J7  | Inlining heuristic: hot callee < 50 nodes, non-polymorphic site, no exception table                | [ ]    |        |
| J8  | Vectorisation: int32-array hot loops lowered to NEON / AVX-2                                       | [ ]    |        |
| J9  | Ahead-of-time compile of startup modules (baseline-only snapshot, like V8 code cache)              | [ ]    |        |
| J10 | WebAssembly bridge: JIT JS ↔ Wasm without trampoline overhead                                       | [ ]    |        |

---

## Track W — Web APIs

**North star:** every Web API a modern server runtime needs, standards-faithful, in `otter-web`.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| W1  | URL + URLSearchParams (WHATWG URL spec, UTS#46 IDNA)                                                | [ ]    |        |
| W2  | fetch + Headers + Request + Response (HTTP/1.1 + HTTP/2 + HTTP/3 via `reqwest`/`hyper`)            | [ ]    |        |
| W3  | WHATWG Streams: ReadableStream, WritableStream, TransformStream, `pipeTo`/`pipeThrough`            | [ ]    |        |
| W4  | WebCrypto (SubtleCrypto): AES, RSA, ECDSA, ECDH, HMAC, SHA-*, HKDF, PBKDF2                         | [ ]    |        |
| W5  | WebSocket client (RFC 6455) + server                                                                | [ ]    |        |
| W6  | Web Workers: `new Worker("./x.js")`, `postMessage`, structured clone, per-isolate worker threads    | [ ]    |        |
| W7  | TextEncoder/TextDecoder/TextDecoderStream (full encoding-spec coverage, not just UTF-8)             | [ ]    |        |
| W8  | File + Blob + FormData + URL.createObjectURL                                                        | [ ]    |        |
| W9  | WebAssembly: `WebAssembly.compile`, `instantiate`, `Memory`, `Table`, streaming                     | [ ]    |        |
| W10 | AbortController + AbortSignal propagating into fetch/streams/setTimeout (no orphan tasks)           | [ ]    |        |

---

## Track N — Node.js compatibility

**North star:** `npm install` + run the top 1000 npm packages unchanged. `node:*` built-ins spec-compliant enough for express, fastify, prisma, drizzle.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| N1  | `node:fs` + `node:fs/promises` — full surface, streams-integrated                                   | [ ]    |        |
| N2  | `node:path` + `node:url` + `node:querystring`                                                       | [ ]    |        |
| N3  | `node:http`/`node:https`/`node:http2` — both client and server                                      | [ ]    |        |
| N4  | `node:net` + `node:tls` + `node:dns` (resolver, lookup)                                             | [ ]    |        |
| N5  | `node:stream` (classic Readable/Writable/Duplex/Transform + web-stream interop)                     | [ ]    |        |
| N6  | `node:buffer` — Buffer as Uint8Array subclass with Node method surface                              | [ ]    |        |
| N7  | `node:crypto` (Node API surface backed by WebCrypto + OpenSSL bindings for the leftovers)           | [ ]    |        |
| N8  | `node:child_process` — `spawn`, `exec`, `fork` (fork spawns a fresh isolate with IPC)               | [ ]    |        |
| N9  | `node:worker_threads` — parity with W6 (Web Workers) under a node-shaped API                        | [ ]    |        |
| N10 | Top-1000 npm package install + run smoke test (automated CI matrix)                                  | [ ]    |        |

---

## Track D — Developer experience

**North star:** the most ergonomic JS runtime to debug, profile, and iterate against. Reviewers see the DevTools protocol + tracing working out of the box.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| D1  | REPL (`otterjs repl`): line editor, multi-line, history, tab completion via shape metadata         | [ ]    |        |
| D2  | Source maps (`SourceMap.from_file`, embedded in Module, used by stack traces)                       | [ ]    |        |
| D3  | Chrome DevTools Protocol (CDP) server: breakpoints, step, watch, scope inspect                     | [ ]    |        |
| D4  | Sampling CPU profiler (Chrome `.cpuprofile` output)                                                 | [ ]    |        |
| D5  | Memory profiler UI (G7 + G8 → shippable `.heapsnapshot`)                                           | [ ]    |        |
| D6  | Coloured diagnostics (error with code frame + source snippet, miette-powered)                      | [ ]    |        |
| D7  | Tracing (OpenTelemetry spans from runtime + native bindings for user code)                         | [ ]    |        |
| D8  | `--inspect` flag that opens a CDP port; attaches Chrome DevTools or VS Code JS Debugger             | [ ]    |        |
| D9  | `otterjs doctor`: health check (fs caps, net caps, heap cap, GC stats, runtime version)            | [ ]    |        |
| D10 | Hot-reload / HMR primitive (per-module timestamp check + reset, used by dev servers)                | [ ]    |        |

---

## Track T — Tooling

**North star:** ship a first-party package manager, bundler, test runner, formatter, and linter — not a wrapper around npm/esbuild/vitest/prettier/eslint. Per-tool criterion in each row.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| T1  | `otterjs pm` — npm registry fetch + tarball extract + node_modules layout                          | [ ]    |        |
| T2  | `otter.lock` format (deterministic, diffable)                                                      | [ ]    |        |
| T3  | Workspace support (monorepo: `workspaces: [...]` in package.json)                                  | [ ]    |        |
| T4  | `otterjs bundle` — TS/JS bundler backed by oxc, outputs ES modules + source maps                    | [ ]    |        |
| T5  | `otterjs test` — Jest/vitest-compatible runner with snapshot testing + coverage                     | [ ]    |        |
| T6  | `otterjs fmt` — single-pass formatter (Prettier-compatible output for JS/TS)                       | [ ]    |        |
| T7  | `otterjs lint` — rule surface compatible with common eslint rules (`no-unused-vars`, etc.)          | [ ]    |        |
| T8  | Code coverage (statement + branch) with lcov + HTML reporter                                        | [ ]    |        |
| T9  | Bench runner (`otterjs bench`) — criterion-style runner for user code                               | [ ]    |        |
| T10 | `otter.toml` — single config file for every tool (fmt, lint, test, build, bundler settings)         | [ ]    |        |

---

## Track S — Security

**North star:** deny-by-default capability model, per-module permissions, signed lockfiles, audited supply chain. A runtime that's safe for LLM agents to execute.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| S1  | `--allow-read=<path>` / `--allow-net=<host>` / `--allow-env=<var>` (fine-grained grants)           | [ ]    |        |
| S2  | Per-module capabilities via `otter.toml` — dependency tree declares what it needs                  | [ ]    |        |
| S3  | Signed `otter.lock` + package-integrity verification (SHA-256 + minisign)                          | [ ]    |        |
| S4  | Runtime security audit log (every fs/net/env access → structured event stream)                     | [ ]    |        |
| S5  | Sandboxed evaluation primitive for LLM agents (`Otter.runSandbox(code, caps)`)                      | [ ]    |        |
| S6  | Secret-redaction env policy (default deny for `*_SECRET`, `*_TOKEN`, `AWS_*`)                      | [ ]    |        |
| S7  | Supply-chain attestation (SLSA provenance for every package fetched)                                | [ ]    |        |
| S8  | WebCrypto-backed code signing + verification                                                        | [ ]    |        |
| S9  | Per-isolate resource caps (CPU ms, heap bytes, fs bytes written, net bytes sent)                    | [ ]    |        |
| S10 | Capability fuzz harness (CI: drop random grants, expect no out-of-policy I/O)                      | [ ]    |        |

---

## Track F — FFI & native interop

**North star:** calling Rust/C from JS at zero-overhead and vice-versa, with a capability-gated `dlopen`. NAPI-compatible surface so Node native modules run unchanged.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| F1  | `otter:ffi` stable surface: `CFunction`, `linkSymbols`, `JSCallback` (today) → typed signatures     | [ ]    |        |
| F2  | `dlopen` / `dlsym` with capability grant (`--allow-ffi`)                                           | [ ]    |        |
| F3  | NAPI shim (enough for `better-sqlite3`, `bcrypt`, `node-gyp`-built modules)                        | [ ]    |        |
| F4  | Rust-callable: Rust crate calls JS functions through a typed handle                                 | [ ]    |        |
| F5  | Zero-copy ArrayBuffer ↔ `&[u8]` at the FFI boundary                                                | [ ]    |        |
| F6  | Direct WebAssembly component model (WIT bindings → JS)                                             | [ ]    |        |
| F7  | Structured call-profile data for native calls (feeds D4 profiler)                                  | [ ]    |        |
| F8  | Struct layout helpers (`#[repr(C)]` on the Rust side, `Otter.struct(...)` on the JS side)           | [ ]    |        |
| F9  | Async native calls (spawn on a worker thread, return Promise)                                      | [ ]    |        |
| F10 | Panic → JS TypeError (not process abort) for all FFI entry points                                  | [ ]    |        |

---

## Track X — Differentiators

**North star:** reasons to pick Otter over Node/Deno/Bun that nobody else can match.

| ID  | Scope                                                                                               | Status | Commit |
|-----|-----------------------------------------------------------------------------------------------------|--------|--------|
| X1  | `otter:fhir` — FHIR R4/R5 resource types, Bundle validation, FHIRPath evaluator (healthcare-grade) | [ ]    |        |
| X2  | Multi-tenant isolates: one process serves N tenants, each capped + sandboxed, shared nothing        | [ ]    |        |
| X3  | LLM sandbox primitive: `Otter.llmSandbox({ allowedImports, maxMs, maxHeap })` with cleanup guarantee | [ ]    |        |
| X4  | Durable execution: suspend an async function mid-await, resume after restart (CRIU-style snapshot)  | [ ]    |        |
| X5  | `otter:kv` + `otter:sql` — batteries-included storage primitives (today as `otter-modules`)         | [ ]    |        |
| X6  | Edge-runtime mode: single binary, all V8-isolate cost paid up-front, 1 ms warm-start                | [ ]    |        |
| X7  | Built-in observability (OpenTelemetry metrics + traces, zero-config default)                        | [ ]    |        |
| X8  | Deterministic replay: record every I/O boundary, replay offline for debugging                        | [ ]    |        |
| X9  | Native migration helper for common npm frameworks (adapter shims for express/fastify/hono)          | [ ]    |        |
| X10 | `otterjs shell` — zsh-/fish-killer built on JS + capabilities (scripting language = your runtime)   | [ ]    |        |

---

## Execution order

Pick one milestone per track at a time; work multiple tracks in parallel. Prefer milestones that unblock other tracks (G1 → JIT deoptimisation; W2 → N3 HTTP; D3 → everything). Track priorities, not dates — "done when done" with the quality gate green.

First-wave focus (next ~10 commits):
- **E1** — BigInt auto-boxing closes the last v2 gap
- **P1** — polymorphic IC is the biggest single perf lever in the interp
- **G1** — nursery GC unlocks allocation-heavy workloads
- **W1** — URL spec-compliant is the base of W2/N2
- **D2** — source maps are the base of D3/D6
- **T1+T2** — already partially done in `otter-pm`; close the gap

---

## Historical trackers (closed)

- v2 migration (M0–M36): complete. Tracker file deleted 2026-04-19; commit history holds the trail.
- JIT refactor (M_JIT_A → M_JIT_C.3): complete. Tracker file deleted 2026-04-19.
