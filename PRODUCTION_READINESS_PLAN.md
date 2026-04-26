# OtterJS — Production Readiness Plan

> Critical audit and executable roadmap to bring OtterJS to stability and production parity with Node.js / Bun / Deno.
>
> Scope of audit: `crates/otter-{gc,vm,jit,runtime,web,modules,nodejs,profiler,test262}` and `crates/otterjs`.
> Audit date: 2026-04-23.
> Baseline assumptions: single-isolate, single-threaded runtime; no legacy users; `#![forbid(unsafe_code)]` is enforced in `otter-vm` / `otter-runtime`; fresh M0 rewrite still in progress under `docs/bytecode-v2.md`.

## Progress tracker

| ID | Title | State | Notes |
|---:|-------|:-----:|-------|
| S1 | Native loops ignore interrupt flag | ✅ | Closed 2026-04-24. First wave covered SpreadIntoArray, Map/Set constructor iterable absorption, `Object.assign`, and back-edge-adjacent helpers. S1-b added shared `NATIVE_LOOP_POLL_INTERVAL`, proxy trap/helper entry polls, yield* iterator helper polls, Reflect.ownKeys result materialization poll, and typedarray callback/search/sort loop polling. |
| **S2** | **No stack-overflow protection** | ✅ | Landed 2026-04-23. `MAX_JS_STACK_DEPTH=24` guard in `interpreter/mod.rs`; catchable `RangeError("Maximum call stack size exceeded")` at `interpreter/runtime_state/call.rs:288` + `dispatch.rs` `call_direct_bytecode`. 4 regression tests in `source_compiler/tests.rs` (`s2_*`). 926 unit tests green. Cap is conservative because per-activation native frame is 60–100 KB in debug — raising requires **C6**. |
| S3 | OOM flag is advisory | ✅ | Closed 2026-04-24. S3-a added back-edge OOM/interrupt polling; S3-b changed `TypedHeap::alloc()` and VM object allocation wrappers to return `Result`, so hard heap caps fail before object-shell allocation mutates the heap. `reserve_bytes` is now `#[must_use]`. Caveat: a few bootstrap-only installers remain infallible by API shape and should become fallible in a future bootstrap-signature cleanup. |
| S4 | MAP_JIT + W^X on macOS | ✅ | Landed 2026-04-24. `code_memory.rs` now uses `MAP_JIT` + `pthread_jit_write_protect_np` on macOS ARM64, keeps the non-Apple Unix `mprotect(RX)` path, and release workflows sign macOS artifacts with `release/macos-jit-entitlements.plist` when Apple secrets are configured (otherwise warn + package unsigned). 3 S4 regression tests in `otter-jit`; VM/runtime suites and clippy green. |
| S5 | Signal handling / panic catcher | ✅ | S5-a landed 2026-04-23 (SIGINT/SIGTERM → cooperative shutdown). S5-b landed 2026-04-24: native descriptor callbacks wrapped in `catch_unwind` → catchable internal `TypeError`. **S5-c landed 2026-04-26**: workspace `[profile.release]` switched to `panic = "unwind"` so the catch_unwind protection covers release binaries, matching V8/JSC/SpiderMonkey defaults. |
| S6 | Thread-local state in embedding | ✅ | Landed 2026-04-24. Dynamic-import host/registry/referrer and `Math.random` PRNG state moved onto `RuntimeState`. Remaining `source_compiler/mod.rs` thread-locals are per-compile transients (set + cleared within one call) and are safe — audited and documented. |
| S7 | Tokio `std::thread::sleep` | ✅ | S7-a landed 2026-04-24 — `from_current()` uses `park_timeout` so `RunInterrupt::fire()` unparks. **S7-b landed 2026-04-26**: `OtterRuntime::run_module_async` drives the event loop via `tokio::time::sleep` instead of `park_timeout`; embedders running OtterJS inside Axum/Tower no longer need `spawn_blocking` for timer-heavy scripts. Quantum-bounded interrupt polling keeps ^C latency under 50 ms. `run_entry_specifier_async` deferred until F1 (loader is sync). |
| S8 | Regex backtracking watchdog | ✅ | S8-a input-length cap landed 2026-04-24 (1 MB UTF-16). S8-b step-limit landed 2026-04-25 — added `ExecConfig { backtrack_limit: Option<u64> }` + `ExecError::StepLimitExceeded` to `regress` fork (local path override); otter caps per-exec budget at **10 M steps** and surfaces exhaustion as catchable `RangeError("…ReDoS protection")`. Bonus same-session: regex literals now parse-validated by source compiler (otter-side bug — was deferring SyntaxError to first `.exec()`). test262 RegExp: 56.2 % → **68.7 %** (+344 tests), **0 timeouts, 0 crashes**. Upstream `regress` PR pending (user-owned, tracked outside plan). |
| **C4** | **Feedback vector wiring** | ✅ | Landed 2026-04-24. Compiler now allocates Comparison/Branch/Call/Property(store) slots and attaches them at every Test/JumpIf/Call/StaNamedProperty emission site; dispatcher records observations on each lattice. `#[allow(dead_code)]` lifted from `record_*`. 5 regression tests confirm end-to-end population; full VM suite 945/946 (1 ignored). `MAX_JS_STACK_DEPTH` lowered 24→20 (C4's record_* calls bumped per-activation debug frame enough to SIGSEGV the S2 regression test at the old cap). |
| **C10** | **`String.prototype.normalize` returns input unchanged** | ✅ | Landed 2026-04-24. Coerces form arg, dispatches to `unicode_normalization::UnicodeNormalization::{nfc,nfd,nfkc,nfkd}()`, throws catchable `RangeError` for invalid form. 4 regression tests (`c10_*`). |
| **C11** | **`Number.prototype.toString(radix)` falls to exponential for large N** | ✅ | Landed 2026-04-24. Ported V8's `DoubleToRadixCString` — integer part via repeated `floor(n/radix)` (past 2^53 safe), fractional part via delta-bounded repeated multiply with V8-style round-up carry. 5 regression tests (`c11_*`). |
| **C12** | **`BigInt(Number)` truncates via `as i64`** | ✅ | Landed 2026-04-24. Bit-exact IEEE754 → BigInt from `(mantissa|implicit_1) << adjusted_exponent`, sign applied last. Preserves `Math.pow(2,70)` etc. 6 regression tests (`c12_*`). |
| **C9** | **String indexed methods re-encode UTF-16 per call** | ✅ | Landed 2026-04-25. `charAt`/`at`/`codePointAt`/`indexOf`/`lastIndexOf`/`includes`/`startsWith`/`endsWith`/`slice`/`substring`/`padStart`/`padEnd` now read `JsString` WTF-16 backing store directly via `this_js_string_value` + new `arg_js_string_value`. Eliminates per-call `js_to_string` → UTF-8 → `encode_utf16` round-trip and lone-surrogate U+FFFD mangling. Fixes spec correctness for `charAt`/`at` (now index by UTF-16 unit, not Unicode code point — `"😀".charAt(0)` returns `"\uD83D"`). 6 regression tests `c9_*`. test262 String/prototype 68.8 % overall (`charAt` 70.0 %, `indexOf` 74.5 %, `slice` 71.1 %, `at` 54.5 %, `codePointAt` 56.2 %). |
| **C13** | **RegExp captures lossy UTF-16** | ✅ | Landed 2026-04-24. All 3 capture output sites in `regexp_builtin_exec` (full match / positional / named) now build `JsString::from_utf16(&[u16])` and `alloc_js_string`, preserving lone surrogates. `String.prototype.charCodeAt` reads `JsString.code_unit_at` directly (no more UTF-8 round-trip) and `String.fromCharCode` uses `alloc_js_string`. 3 regression tests verify via direct heap inspection (`c13_*`). Caveat: end-to-end lone-surrogate preservation via regex input *still* goes through `js_to_string` → UTF-8 for the haystack; the capture-output fix is half the battle — full WTF-16 regex pipeline is a separate refactor. |
| **C1** | **BigInt represented as decimal string, parsed on every op** | ✅ | Landed 2026-04-25. New `bigint_value::BigIntPayload` enum (`Inline(i64)` fast path + `Heap(Box<BigInt>)`) replaces `Box<str>`. All arithmetic ops (`add`/`sub`/`mul`/`div_trunc`/`rem_trunc`/`pow`/`bitand`/`bitor`/`bitxor`/`shl`/`shr`/`neg`/`bitnot`) carry checked-i64 inline paths and promote/demote canonically (so equality dispatch is just variant check). Discovered & fixed adjacent gaps as part of correctness: `Opcode::Negate` and `BitwiseNot` were forwarding BigInt operands into `js_to_number` (TypeError); `TestLessThan/GreaterThan/LessThanOrEqual/GreaterThanOrEqual` were int32-only and ignored §7.2.13 IsLessThan — now dispatch through it for non-int32 operands. 16 unit tests for `BigIntPayload` + 11 source-level `c1_bigint_*` regression tests. test262: BigInt 40.8% → **47.4%** (+10), addition 87.4% → **91.6%** (+4), language/expressions/less-than & greater-than now exercise BigInt+Number paths. **C1-b** (NaN-box `TAG_BIGINT_SMALL` to skip heap entirely for inline magnitudes) deferred — would touch every Value site and is gated on a bigger value-encoding sweep. |
| **C14 / C-args** | **Arguments object missing entirely (regression from VM rewrite)** | ✅ | Landed 2026-04-25. Audit found `arguments` was COMPLETELY missing from the new VM stack (lost in the otter-vm-core / otter-vm-bytecode merge); MEMORY.md's old test262 numbers reflected the parked stack. Implemented unmapped Arguments exotic object end-to-end: new `Activation::argc` field for call-site argument count, dispatch handler for `Opcode::CreateArguments` (§10.4.4.7), source-compiler `emit_arguments_object` with V8-style "argumentsObjectNeeded" elision via oxc_ast_visit `ArgumentsUseScanner`. Bonus carve-out: `%ArrayIteratorPrototype%.next` now falls back to `LengthOfArrayLike` + `OrdinaryGet` for non-Array iterables (arguments, plain array-likes), and `SpreadIntoArray` falls back to the protocol path when the fast iterator allocator rejects the source. 6 regression tests (`c_args_*`). Test262: RegExp 68.7 % → **70.9 %** (+60), `language/arguments-object` 0 % → **46.7 %** (215/460). Caveat: Mapped variant (sloppy + simple params with UpvalueCell aliasing) reserved for follow-up — imm=1 to `CreateArguments` is unused. |
| **Build** | **Workspace build breakage on parked crates from S3-b** | ✅ | Landed 2026-04-24 as part of this session. S3-b changed `alloc_object` / `alloc_string` / `alloc_native_object(_with_prototype)` / `alloc_host_function` / `alloc_array_with_elements` / `alloc_array_buffer_with_data` / `alloc_typed_array` / `alloc_type_error` to return `Result` but left `otter-nodejs`, `otter-web`, `otter-modules`, and `otter-test262` broken (160+ errors on a clean `cargo build --workspace`). This session threaded fallible allocation through those crates (~27 automated edits plus ~25 hand-edits). CLI now compiles. |

Legend: ☐ pending · 🏗 in progress · ✅ done · ⚠️ partial

---

## 0. TL;DR

**Status: _not_ production-ready for general JavaScript workloads.** Core strengths (spec discipline, solid ES2024 surface in intrinsics, clean Rust boundaries, capability model sketch, strong test262 runner) are offset by three architecture-level gaps that no amount of incremental polish can close:

1. **GC is a dual architecture, wrong side wired.** The sophisticated page-based generational collector in `crates/otter-gc/` (scavenger, write barriers, marking bitmap, incremental drain) is scaffolding. The actually-running GC is `otter_gc::typed::TypedHeap` — a flat `Vec<Option<Slot<Box<dyn Traceable>>>>` doing stop-the-world mark-sweep. Every "production-grade GC" guarantee in `lib.rs:1-18` is aspirational. Pause time @ 1 GB heap: 10–100 ms with **no** incrementality, **no** generational reclamation, **no** barriers.  See §3.2.
2. **Interpreter cannot be interrupted reliably, cannot survive deep recursion, and cannot bound native-loop work.** The watchdog relies on cooperative `check_interrupt()` at bytecode back-edges (`interpreter/mod.rs:1423-1429`); iterator spread, property iteration, `own_property_keys`, proxy trap dispatch, and JSON / regex native loops are **not** polled. There is **no** MAX_STACK_DEPTH anywhere — deep recursion overflows the native Rust stack and aborts the process. See §2.1, §2.3.
3. **JIT is single-tier baseline covering ~20 % of the Ignition opcode set.** No optimizing tier, no property IC in emitted code, no call-site specialization, no object materialization on deopt. Any realistic workload (property access, method calls, string ops, closures) never enters JIT — those opcodes are unsupported and the function is rejected. MAP_JIT / `pthread_jit_write_protect_np` are **not** called on macOS Apple Silicon despite the comment at `baseline/mod.rs:3670-3671` claiming otherwise. Any hardened-runtime build will segfault on first JIT invocation. See §4.

Secondary issues that will bite in weeks, not months:

- `fetch()` is synchronous and blocks the event loop for up to 30 s (`otter-web/src/request_response_api.rs:12,292`). Any HTTP-heavy workload deadlocks.
- `thread_local!` state in `module_loader.rs:27-53` breaks embedding into multi-threaded hosts (Axum, Tower, Actix).
- No `process` global, no `fs`, no `path`, no `os`, no `Buffer`. `otter-nodejs` is mounted at CLI startup (`otterjs/src/main.rs:504`) but contradicts `CLAUDE.md`'s "parked" directive — it is **active**, just incomplete (`process.argv`/`process.env` only).
- Eval recompiles from source on every call (`interpreter/runtime_state/eval.rs:40`). No bytecode cache.
- String concat is eager and allocating (`js_string.rs:305-311`) — no rope, no CoW substring, no intern pool for runtime-created strings. Loops that concat produce O(n²) heap traffic.

Everything actionable is tracked in §8–§10 below.

---

## 1. Codebase reality check

The workspace is mid-rewrite. `AGENTS.md` lists the active stack as `otter-gc → otter-vm → otter-runtime → otter-jit`, and `otter-nodejs`/`otter-node-compat` are declared parked in `CLAUDE.md`. Reality diverges:

| Crate | LOC | Status claim | Actual status |
|-------|-----|--------------|---------------|
| `otter-gc` | ~3000 | Generational + incremental + barriers (`lib.rs:1-18`) | Only `typed.rs` is wired; rest is reference impl / tests |
| `otter-vm` | ~60k | `#![forbid(unsafe_code)]`, M0 transitional | Correct, but `#![allow(dead_code)]` + many `#[allow(dead_code)]` markers flag large unwired feedback pipeline (`interpreter/frame_runtime.rs:67-141`) |
| `otter-jit` | ~6k | "Three-tier JIT" (`ROADMAP.md` Track J) | Baseline-only; Cranelift retired (`pipeline.rs:1-12`); covers ~20 % of opcodes |
| `otter-runtime` | 5029 | Public API layer | Functional; wires extensions, capabilities, event loop |
| `otter-web` | 4979 | WHATWG Web APIs | `URL`, `URLSearchParams`, `TextEncoder/Decoder`, `Headers`, `Blob`, `FormData`, `Request/Response`, blocking `fetch` |
| `otter-modules` | 3447 | `otter:kv`, `otter:sql`, `otter:ffi` | All present; FFI uses `libffi` + `libloading`; cap gated (`ffi.rs: require_ffi`) |
| `otter-nodejs` | 3505 | **Parked** (`CLAUDE.md`) | **Active** — hooked in CLI (`main.rs:504`); `process`, `path`, `util`, `assert`, `fs` (463 LOC stub), `net`/`http` (151+65 LOC stubs) |
| `otter-node-compat` | 2 files | **Parked** | Test harness that runs Node's tree against Otter — not a runtime API |
| `otter-profiler` | 1367 | CPU + heap + async trace + stats | Present, but no CDP / Inspector bridge |
| `otter-test262` | 2703 | test262 conformance runner | Healthy; `timeout_secs=10`, `max_heap_bytes_per_test=512MB`, ignored-test / known-panic lists |
| `otter-pm*` | — | Package manager | Track T1/T2 landed per ROADMAP; workspace, build, lint etc. not started |

**Takeaway for planning:** MEMORY.md and parts of CLAUDE.md are stale with respect to the active crates. Several "done" entries in ROADMAP.md (P1 Polymorphic IC is marked [x] with commit `d06d2fc`) coexist with dead feedback scaffolding — feedback is recorded *per frame* but not merged back into the persistent feedback vector on every path (`interpreter/frame_runtime.rs` — all seven `record_*` methods carry `#[allow(dead_code)]`). **Don't trust the state matrix; read the code.**

---

## 2. Stability & safety — detailed findings

### 2.1 Watchdog coverage is cooperative and incomplete

The VM's watchdog is an `Arc<AtomicBool>` cooperative flag (`interpreter/mod.rs:159`), polled by:
- Every bytecode back-edge (`interpreter/mod.rs:1423-1429`).
- Event-loop driver (`otter-runtime/src/runtime.rs:578,587,613`).
- Generator resume (`runtime_state/iterators.rs`).
- `sleep_until_interruptible` (`runtime.rs:568-578`).

**Native loops that bypass the flag** (file:line — loop description):

| Site | File:line | Consequence |
|------|-----------|-------------|
| SpreadIntoArray | `interpreter/dispatch.rs:1743-1752` | `[...iter]` on a hostile `{ next: () => ({done:false, value:0}) }` iterator hangs forever |
| Own-property key enumeration | `runtime_state/mod.rs:686-703` | `Object.keys(hostile)` / `for (k in hostile)` uninterruptible across proto chain |
| String exotic index walk | `runtime_state/mod.rs:649-658` | `Object.getOwnPropertyNames(new String(longStr))` blocks for string length |
| Proxy trap dispatch | `runtime_state/proxy.rs` (whole file) | Deep / recursive proxy chains: no ceiling on trap depth |
| Generator delegation (`yield*`) | `runtime_state/iterators.rs:110-150` | Adversarial `return`/`throw` forwarding can run unbounded |
| JSON.stringify / parse | `intrinsics/json.rs` | Recursive, no watchdog poll; depth-limit present but no interrupt check |
| Regex execution | `intrinsics/regexp_class.rs:449-453` | `regress` has no watchdog hook — pathological pattern hangs until thread kill |
| Sort / reduce native loops | `intrinsics/array_class.rs` (sort, reduce, flat) | Large arrays with comparator hostilely slow: no intermediate poll |

**Root-cause fix, not whack-a-mole.** Introduce a single "may-yield" checkpoint helper (`runtime.should_yield() -> Result<(), VmError::Interrupted>`) and insert it at the head of every loop that iterates user data. Loops over compile-time-bounded data (e.g. `SmallVec<[Value; 8]>` argument unpack) are exempt.

### 2.2 OOM guardrails are advisory and bypassable

`TypedHeap::alloc()` (`otter-gc/src/typed.rs:233-262`) checks the heap cap and sets the OOM flag **but still allocates**:

```rust
if self.would_exceed_limit(size) {
    self.oom_flag.store(true, Ordering::Relaxed);
}
// … allocation continues unconditionally
```

Callers must poll the flag at safepoints. A tight allocation loop (e.g. `Array.from({length: 2**20})`) can overshoot the cap by hundreds of MB before the flag is observed. `test262-safe.sh` mitigates this with an OS-level `ulimit -v`, but that kills the process — not catchable as `RangeError`. Production workloads have neither outer `ulimit` nor an external watchdog.

Secondary: `Vec`-backed container growth (e.g. `JsObject.values`, array elements, string buffers, BigInt digits) is only accounted for when code manually calls `TypedHeap::reserve_bytes()`. A forgotten reservation site silently bypasses the cap. There is **no test** that every container-growth site reserves — this is a protocol enforced by convention.

### 2.3 Stack overflow protection: **absent**

Grep for `max_stack_depth`, `stack_overflow`, `recursion_depth`, `stacker::` across `crates/otter-vm/**`, `crates/otter-runtime/**`: 0 hits. Deep JS recursion (factorial without TCO, mutual recursion, pathological JSON) will exhaust the Rust thread stack (default 2 MB) and `SIGSEGV` the process. V8 raises `RangeError: Maximum call stack size exceeded`; Otter aborts.

`frame.rs` / `interpreter/activation.rs` expose the frame-allocation API; there is no counter per execution context and no guard. **High-severity**: one-line JS `function f(){f()}; f()` kills the host.

### 2.4 Signal handling: **absent**

No `ctrl-c`, `signal-hook`, `SIGINT`, `SIGTERM` or panic-to-JS bridge anywhere in the workspace (`rg "signal|SIGINT|SIGTERM|ctrlc"` over `crates/otterjs`, `crates/otter-runtime` returns only a code comment in `runtime.rs:267`). Consequences:

- `^C` in CLI = kernel kill. JIT-mapped pages are leaked, `pthread_jit_write_protect_np` state not flipped back — irrelevant at process exit, but breaks any in-process embedder that re-creates runtimes.
- `std::panic::catch_unwind` is not wrapped around interpreter re-entry. A bug in a native intrinsic panics the whole process (`test262` profile explicitly sets `panic = "unwind"` and uses `catch_unwind`, confirming the rest of the stack is *not* unwind-safe).

### 2.5 Unsafe surface audit

Count of `unsafe` blocks by active crate:

| Crate | unsafe fn | unsafe block | unsafe impl | Notes |
|-------|-----------|--------------|-------------|-------|
| `otter-vm` | 0 | 0 | 0 | `#![forbid(unsafe_code)]` (`lib.rs:7`) |
| `otter-runtime` | 0 | 0 | 0 | `unsafe_code = "forbid"` (`Cargo.toml:10`) |
| `otter-gc` | 6 | ~40 | 2 | Page header extraction (`page.rs:304-318`), free-list pointer math (`space.rs:298-341`), marking tracer (`trace.rs:86-180`). Each block annotated with `// SAFETY:` — good hygiene |
| `otter-jit` | 3 | 14 | 2 | mmap / mprotect (`code_memory.rs:50-95`), raw `RuntimeState*` cast (`tier_up_hook.rs:84,133,202,296,352`), `CompiledFunction: Send+Sync` without auditable rationale beyond "VM owns uniquely" |
| `otter-modules/ffi.rs` | 2 | 8 | 4 | libffi boundary (`ffi.rs:147-195`); `CifContext: Send+Sync`; `JsCallbackData: Send+Sync` — **dubious**, these traits are asserted on data that holds a `&mut RuntimeState` pointer |

Biggest concrete red flag: `otter-modules/src/ffi.rs:172-177`

```rust
unsafe fn current_ffi_runtime<'a>() -> Option<&'a mut RuntimeState> {
    // … returns &mut from a thread-local raw pointer
}
```

This re-entry pattern would be sound if guarded by a `!Sync` token, but `JsCallbackData` asserts `Send + Sync`. If anything hands a JS callback to a library that invokes it from a worker thread (libffi closures in particular — the whole point of `ffi_closure`), we have undefined behaviour. **Treat as data race in waiting.**

### 2.6 MAP_JIT is not used on macOS

`crates/otter-jit/src/code_memory.rs:50-60` calls `mmap(PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANON)`. There is no `MAP_JIT` flag, no `pthread_jit_write_protect_np()` call, and no JIT entitlement request. The comment at `baseline/mod.rs:3670-3671` claims the production path handles MAP_JIT correctly — grep disagrees.

Consequence on Apple Silicon under hardened runtime (every signed Mach-O built with Xcode / `codesign --options=runtime`): the first JIT invocation fails with `EXC_BAD_ACCESS` because PROT_EXEC without MAP_JIT is denied on ARM64 macOS. **Any release build shipped to macOS users is broken.** Unsigned dev builds happen to work.

Linux arm64: `flush_instruction_cache()` is a no-op on non-Apple targets (`code_memory.rs:136-137`). Any JIT on arm64 Linux has a non-zero chance of executing stale I-cache lines.

Windows: unimplemented (`code_memory.rs:96-100`).

### 2.7 Thread-local state crossed with embedding

Three `thread_local!` sites survive:

1. `crates/otter-vm/src/module_loader.rs:27-32` — `DYNAMIC_IMPORT_HOST`, `DYNAMIC_IMPORT_REGISTRY` as `RefCell<Option<…>>`. Set during dynamic `import()`, read by the module graph driver. Crashes or corrupts state if the embedder calls `import()` concurrently from multiple threads sharing one runtime.
2. `crates/otter-vm/src/intrinsics/math.rs:717` — PRNG state (`thread_local! { static RNG … }`). Determinism break across threads.
3. `crates/otter-vm/src/source_compiler/mod.rs:1162` — per-thread compile sink (interning).

None of these are fatal for the single-thread CLI, but all are tripwires for the "multi-tenant isolates" track (Track X2 in `ROADMAP.md`).

---

## 3. Architecture vs. Node / Bun / Deno — gap matrix

### 3.1 Value representation

| Dimension | V8 | JSC | Bun (JSC) | Deno (V8) | Otter | Gap |
|---|---|---|---|---|---|---|
| Encoding | Pointer compression on 64-bit | JSValue64 NaN-box | JSValue64 | Pointer compression | NaN-box (`value.rs:28-44`), 8 B, `Copy` | None — good baseline |
| Smi fast path | 31-bit on 64-bit | 32-bit | 32-bit | 31-bit | 32-bit `TAG_INT32` (`value.rs:94-96`) | Good |
| Smi/-0 handling | Correct | Correct | Correct | Correct | Correct but inefficient check order (`value.rs:111-116`) | Minor perf |

### 3.2 GC

| Dimension | V8 Orinoco | JSC Riptide | Otter (shipping) | Otter (designed but not wired) |
|---|---|---|---|---|
| Allocation | Bump-pointer, <10 cycles | Bump + block | `Box<dyn Traceable>` via `Vec::push`, ~50 cycles (`typed.rs:233-262`) | Bump via `GcHeap::alloc_young` (`heap.rs`) — **not called by VM** |
| Generational | Yes | Yes | **No** — all objects `is_young=true` forever | Scavenger implemented (`scavenger.rs:75-190`) — unused |
| Incremental marking | Yes, budgeted | Yes | **No** — full STW | `drain_with_budget` (`marking.rs:117`) — unused |
| Write barriers | Yes (generational + incremental) | Yes | **None** | Implemented (`barrier.rs:84-164`) — zero call sites in VM |
| Concurrent marking | Yes | Yes | **No** | Prepared (`header.rs` AtomicU8) — single-threaded |
| Compaction | Parallel sliding | Yes | **No** | Not designed |
| Ephemerons | Yes (fixpoint) | Yes | Declared (`object.rs:228-247`) but no fixpoint loop | API exists (`typed.rs:306-319`), uncalled |
| FinalizationRegistry | Yes | Yes | Declared enum (`object.rs:246`), **no scheduling** | — |
| Heap snapshot | `.heapsnapshot` (CDP) | Proprietary | **None** | — |
| Pause target @ 1 GB | <10 ms | <10 ms | 10–100 ms realistic (full STW on full slot array `typed.rs:378-387`) | — |

**This is the single biggest architectural bet to close.** Either (a) delete the page-based GC and commit to a correct STW mark-sweep + incremental-marking retrofit on `TypedHeap`, or (b) finish the `GcHeap` integration and move every `TypedHeap` use-site to it. Option (b) is the expensive-but-right path.

### 3.3 Bytecode interpreter

| Dimension | V8 Ignition | JSC LLInt | Otter |
|---|---|---|---|
| Dispatch | Accumulator + threaded | Accumulator + LLInt threaded | Accumulator, switch-dispatched with per-opcode fast-path inlining (`dispatch.rs:57-2909`) |
| Hot-op inlining | Yes (computed goto) | Yes | Yes for top ~10 opcodes |
| Feedback vector | Dense per function | Dense per function | `FeedbackVector` (`feedback.rs`) — structure exists, half the recording paths dead (`frame_runtime.rs:67-141` all `#[allow(dead_code)]`) |
| IC slots | Monomorphic → polymorphic (4) → megamorphic | Similar | Same lattice for property loads (`feedback.rs:251 POLYMORPHIC_MAX_SHAPES = 4`) |
| IC coverage | Load, store, method, element, call, typeof, eq, add | Same | **Only property load** has IC. Store, method, element, typeof, comparison, arithmetic feedback is recorded but not consumed on the hot path |
| Debug support | Source maps + inspector | Source maps + inspector | Source maps present (`source_map.rs`, D2 landed per ROADMAP); inspector **absent** |

### 3.4 JIT

| Dimension | V8 | JSC | Otter |
|---|---|---|---|
| Tiers | Sparkplug → Maglev → Turbofan | Baseline → DFG → FTL | Baseline only (`lib.rs:1-7`) |
| Opcode coverage | ~95 % | ~90 % | ~20 % — int32 arithmetic, jumps, loads, `Return` (`baseline/mod.rs:212-225`) |
| Property IC in code | Yes, inline-patched | Yes | **No** |
| Call-site IC | Yes | Yes | **No** (`CallDirect` is a deopt boundary) |
| OSR | Yes, to higher tier | Yes | Partial — interp→baseline only, limited first-op filter (`baseline/mod.rs:819-843`) |
| Deopt | Full frame descriptors | Stack maps | Bailout sentinel (`deopt/bailout.rs:1-58`); no object materialization |
| Type feedback | Full | Full | Arithmetic only (`Int32 / Number / Any` lattice) |
| Inlining | Speculative | Speculative | **None** |
| Register allocation | Graph + linear scan | Graph | Fixed pinning (`arch/aarch64.rs`, ~5 reg + 4 loop-candidate) |
| W^X | All platforms | All platforms | Partial — no MAP_JIT on macOS, no I-cache flush on non-Apple arm64 (`code_memory.rs:96-137`) |
| Code cache eviction | LRU + size limit | Tier-based + pool | Manual `flush_cold` / `flush_unstable` — `code_cache_limit_bytes` **unenforced** (`code_cache.rs:48-56`) |

Net: on anything more interesting than integer Fibonacci, Otter is interpreter-only. Baseline JIT on arithmetic loops buys 2–3× over interp; Bun is 50–100× on the same inputs. **Either commit to building Maglev-class tier 2 or stop funding the baseline and invest in interpreter throughput.**

### 3.5 Async & event loop

| Dimension | Node | Bun | Deno | Otter |
|---|---|---|---|---|
| Event loop | uv_loop (epoll/kqueue/iocp) | custom + zig/uring | Rust Tokio | Tokio current-thread **or** `from_current` fallback (`event_loop.rs:274,289`) |
| Microtask ordering | nextTick → promise → queueMicrotask | promise → microtask | promise → microtask | All three queues exist (`microtask.rs:65-75`) — explicit priority drain helper missing |
| Timers | timing wheel | timing wheel | Tokio delay + BinaryHeap | BinaryHeap + `HashMap` cancel index (`event_loop.rs:62-237`); `next_deadline` is O(n) skipping cancelled |
| I/O | Fully async | Fully async | Fully async | **Sync** — `fetch()` is blocking (`request_response_api.rs:292`) |
| AbortSignal | Yes | Yes | Yes | **Missing** |
| Embedded mode | Via napi | Via bun runtime | Via deno_core | `from_current()` path calls `std::thread::sleep` inside tokio reactor (`event_loop.rs:376`) — **blocks host runtime** |

### 3.6 Modules

| Dimension | Node | Bun | Deno | Otter |
|---|---|---|---|---|
| Resolution algorithm | Node algorithm | Node + own | Deno URL resolver | `oxc_resolver` — diverges from Node edge cases (index fallback, `"main"` field) |
| Dynamic `import()` | Yes | Yes | Yes | Yes, but **thread-local context** (`module_loader.rs:27-32`) breaks under concurrent callers |
| `import.meta.resolve` | Yes | Yes | Yes | **No** |
| `node:` scheme | Native | Native | Limited | Routed through `otter-nodejs` extension, per-module |
| `data:` URL | Yes | Yes | Yes | **No** |
| CommonJS `require` | Native | Shim | Shim | **Absent** |
| `.ts` handling | No (loader) | Native via oxc | Native via swc | Native via oxc — no transpile cache |
| Source cache | In-process | In-process | In-process | 256-entry LRU, silent eviction (`otter-runtime/src/host/module_loader.rs:114-150`) |

### 3.7 Strings

| Dimension | V8 | Otter | Gap |
|---|---|---|---|
| Encoding | OneByte (Latin-1) / TwoByte (WTF-16) | WTF-16 only (`js_string.rs:25`) | Memory 2× on ASCII-heavy code |
| Ropes / ConsString | Yes, lazy | **No** — eager concat (`js_string.rs:305-311`) | Loops `s += piece` are O(n²) |
| SSO (inline short string) | Yes (<8B) | **No** (`Box<[u16]>` always) | Memory pressure for property names |
| Runtime intern pool | Yes (global) | Per-function `StringTable` only (`string.rs`) | Duplicate allocations on concat results |
| `charCodeAt` / `codePointAt` | O(1) after first | **O(n)** — encodes full UTF-16 per call (`intrinsics/string_class.rs:471`) | Pathological in tokenizers |
| `toLowerCase` | In-place / alloc once | Round-trips UTF-16→UTF-8→UTF-16 (`js_string.rs:330-339`), loses lone surrogates | Correctness gap |
| `String.prototype.normalize` | Correct | **Returns input unchanged** (`intrinsics/string_class.rs:1333`) despite `unicode-normalization` crate in deps | Spec violation |
| Hash-accelerated equality | Yes (cached hash + length prefix) | Plain slice eq (`js_string.rs:364-368`) | Property-key lookups slow |

### 3.8 Numbers / BigInt

| Dimension | V8 | Otter | Gap |
|---|---|---|---|
| BigInt repr | `Vec<u64>` limbs | Heap-stored decimal string; parsed to `num_bigint::BigInt` on every op (`intrinsics/bigint_class.rs:279-282`) | Fundamental: every op is string-parse + compute + stringify |
| Small BigInt opt | u64 inline | **None** | |
| `Number#toString(r)` for r≠10, large N | Correct | Falls through to exponential notation (`intrinsics/number_class.rs:425-429`) | Spec violation |
| `Number` → `BigInt` | Correctly rejects out-of-safe-range | Casts through `i64` (`intrinsics/bigint_class.rs:265`) — truncates for 9.223e18 | Correctness bug |

### 3.9 RegExp

- Engine: `regress 0.11.1` with `utf16` feature — correct choice for JS semantics.
- **No compilation cache** (`intrinsics/regexp_class.rs:397-404`): `regexp_builtin_exec` recompiles pattern on every call. Patterns in hot paths are re-parsed tens of thousands of times per second.
- **Input re-encoded to UTF-16 per exec** (`intrinsics/regexp_class.rs:426`).
- **No backtracking watchdog**: pathological `(a+)+$` against a 40-char input hangs the interpreter.
- Capture groups go through `String::from_utf16_lossy` (`intrinsics/regexp_class.rs:497,516,529`), so lone surrogates in captures are replaced with U+FFFD — breaks WTF-16 contract.

### 3.10 Observability

| Feature | Node | Deno | Otter |
|---|---|---|---|
| CDP / Inspector | Yes | Yes | **None** (`rg "Inspector|CDP"` → 0 hits) |
| CPU profile (.cpuprofile) | Yes | Yes | Yes (`otter-profiler/cpu.rs:to_cpuprofile`) — not wired to inspector |
| Heap snapshot (.heapsnapshot) | Yes | Yes | Partial (`otter-profiler/memory.rs:to_heapsnapshot`) — no walker over `TypedHeap` slots |
| Async stack trace continuity | Yes | Yes | Partial (`otter-profiler/async_trace.rs`) — no inspector integration |
| Prometheus / OpenTelemetry | via modules | via modules | **None** |
| `--inspect` flag | Yes | Yes | **Absent** in CLI (`otterjs/src/main.rs` — no flag) |

### 3.11 Process / system

Not available as builtins unless the user opts into `otter-nodejs`:
- `process.argv`, `process.env`: available via `otter-nodejs/src/process.rs`.
- `process.exit`, `process.on('exit'|'SIGINT'|…)`: not found.
- `Buffer`: not implemented anywhere — users must use `Uint8Array` directly.
- `fs.*`: partial stubs in `otter-nodejs/src/fs.rs` (463 LOC).
- `child_process.spawn`: stubs only (`otter-nodejs/src/child_process.rs` — 65 LOC).
- `net` / `http` / `https` / `tls` / `dns` / `zlib`: all stubs or missing.
- `worker_threads`: stub (`worker_threads.rs` — 81 LOC) — no actual thread spawn.

---

## 4. Performance findings

Hard numbers vs. Bun/Node are not measured (no benchmark harness lives in repo under `benches/`). The concrete hot-path observations:

- **Interpreter dispatch**: switch-match with inline fast paths for top ~10 opcodes. Absent: computed-goto / tail-calls, instruction fusion, threaded prefetch. Estimated 30–50 % headroom vs. Ignition.
- **Property access**: 2 cache misses (keys `Vec` + values `Vec`) per read on `JsObject` (`object.rs:900-920`). No in-object slots. Hidden classes are **monotonic IDs** (`object.rs:8028-8032`) — no transition graph, shape cache, or lookup inlining. Expected 2–3× slowdown vs. V8 on property-heavy microbenchmarks.
- **Function call**: `Activation::new` boxes `registers` and `open_upvalues` per call (`interpreter/activation.rs:139-141`). TailCall replaces fields in place but does **not** reuse boxes. Generator resume clones the entire `Activation` (`derive(Clone)` at `activation.rs:55`). Per-call malloc on every non-tail path.
- **Call-spread / iterator-spread**: `Vec::with_capacity(len)` per call (`dispatch.rs:1334, 1748`) — no inline small buffer. Every 0–8-arg call allocates.
- **Closures**: upvalues stored in `UpvalueCell` heap values; any outer-binding mutation traces through GC cell. No write-barrier optimisation (no barriers exist).
- **Promises**: no per-element function pooling for `Promise.all` / `allSettled` / `race` (intrinsics register fresh callbacks — verify when implementing §P2-C3).
- **String concat**: documented §3.7.
- **JSON**: direct-shape fast path exists (`MEMORY.md` note, confirmed in `intrinsics/json.rs` bulk-copy path); good.
- **Module caching**: 256-entry silent-eviction LRU — fine for CLI, broken for large monorepos.

**Perf blockers ranked by impact:**

1. GC replacement (stop-the-world pauses dominate tail latency).
2. Hidden-class transition graph + shape-aware property slots.
3. Feedback vector coverage for call / store / compare / arithmetic in dispatch (it's recorded, but not consumed).
4. JIT tier 2 or interp fast-path expansion.
5. String rope + Latin-1 encoding.
6. RegExp compile cache.
7. Eval bytecode cache.
8. Per-call allocation elimination (arena for `Activation`).

---

## 5. Developer experience

**Strengths:**
- Miette-powered diagnostics (`otterjs/src/main.rs:render_miette`) are genuinely nice; on par with Deno.
- Source-map-aware stack frames (D2 landed).
- CLI capability flags mimic Deno's, feel familiar.
- Profiling infra (`otter-profiler`) exists even if not surfaced.

**Gaps:**
- **No REPL** (`main.rs:301 command_temporarily_disabled("repl")`).
- **No test runner** (`main.rs:302 command_temporarily_disabled("test")`).
- **No `--inspect`**, no CDP, no VS Code Debug Protocol.
- **No watch mode** (declared flag on Test command, never implemented).
- **No `otter doctor`** health check.
- Error messages: V8-shaped for `TypeError` / `ReferenceError` / `RangeError`, but `Error.captureStackTrace` / `err.stack` V8-extension is absent, so any framework that formats Node-style stacks will mis-render.
- **No canonical benchmark suite** — no `benches/*.rs` under workspace crates that isn't dead. No comparison harness vs. Node / Bun / Deno.

---

## 6. Testing / conformance baseline

- test262 runner is healthy (`crates/otter-test262/src/main.rs` 906 LOC). Heap cap + `ulimit -v` defence (`scripts/test262-safe.sh`) is good thinking.
- Skip list is pragmatic (Atomics / SharedArrayBuffer / Intl edge cases / regress-lib quirks).
- `ignored_tests` includes yield*-delegation, destructuring iterator-close, Annex B HTML comments — these are **real engine gaps**, hiding behind an ignore flag.
- `known_panics` has the RegExp property-escapes pathological case — this is where a real backtracking limit or regex watchdog belongs.
- Unit-test count: `cargo test -p otter-vm --lib` reports 920 passing. Not exercised: multi-thread embedding, signal handling, OOM recovery, MAP_JIT path on macOS, JIT deopt under GC pressure.

---

## 7. Security

Capability model (`HostConfig`, `Capabilities`, `EnvStoreBuilder`) is the right shape and is *enforced* at crate boundaries in `otter-modules/ffi.rs::require_ffi` etc. Gaps:

- Per-module capabilities (S2 in ROADMAP) not implemented — all code in one runtime shares one grant.
- Signed lockfile (S3) not implemented; `otter-pm-lockfile` emits plain TOML.
- No audit log (S4).
- No sandbox primitive (`Otter.runSandbox`) exposed to JS (S5).
- Env-secret deny regex: `AWS_*`, `*_SECRET*` patterns documented in `DEFAULT_DENY_PATTERNS` — confirmed in `otter-runtime/src/host/env.rs`.
- FFI dlopen path does not apply library path allowlist — `DynLib::new(path)` (`otter-modules/src/ffi.rs:183`) takes any user-supplied path as long as `allow-ffi` was granted. Fine-grained "allow this specific shared object only" is missing.

---

## 8. Prioritized executable plan

Effort: **S**=<3 days, **M**=1–2 weeks, **L**=1+ month, **XL**=quarter+.
Risk is called out where non-obvious.

---

### P0 — Stability blockers (must fix before any prod claim)

#### [S1] Native loops ignore interrupt flag → DoS ✅ DONE (closed 2026-04-24)

S1-a (2026-04-23) covered SpreadIntoArray + Map/Set constructor + Object.assign. S1-b (2026-04-24) added shared `NATIVE_LOOP_POLL_INTERVAL = 4096`, proxy trap polls, yield* iterator helpers, Reflect.ownKeys, TypedArray loops. 4 regression tests `s1_*`. See Progress tracker + Appendix C.

#### [S2] No stack-overflow protection ✅ DONE (2026-04-23)

`MAX_JS_STACK_DEPTH = 20` (was 24, lowered by C4) + `RuntimeState::call_depth` guard. StackOverflow → catchable `RangeError("Maximum call stack size exceeded")`. 4 regression tests `s2_*`. Cap is conservative pending **C6** frame slimming. Follow-ups: **S2-b** native-intrinsic recursion guards, **S2-c** runtime-config option.

#### [S3] OOM flag is advisory, containers growing past cap ✅ DONE (2026-04-24)

S3-a (2026-04-23) added back-edge `poll_back_edge()` for OOM/interrupt. S3-b (2026-04-24) made `TypedHeap::alloc()` return `Result<Handle, OutOfMemory>` and threaded fallible allocation through every VM/runtime call site. Caveat: a few bootstrap installers stay infallible by API shape.

#### [S4] MAP_JIT + pthread_jit_write_protect_np missing on macOS ✅ DONE (2026-04-24)

`code_memory.rs` uses `MAP_JIT` + `pthread_jit_write_protect_np` on macOS ARM64, keeps the Unix `mprotect(RX)` path elsewhere. Release workflows sign macOS artifacts with `release/macos-jit-entitlements.plist` (warns + ships unsigned if Apple secrets absent). 3 regression tests `s4_*`. Linux arm64 I-cache flush + Windows allocation remain separate hardening.

#### [S5] Signal handling ⚠️ PARTIAL (S5-a + S5-b done 2026-04-24; S5-c pending)

S5-a: process-wide `ACTIVE_INTERRUPTS` registry + `signal_shutdown()`; CLI wires SIGINT/SIGTERM to cooperative shutdown, double-^C escalates to `exit(130)`. S5-b: `catch_unwind` around native descriptor dispatch in `RuntimeState::call_host_function` for unwind builds.

**Scope decision:** This deliberately does **not** change workspace `[profile.release] panic = "abort"`. The landed protection applies to dev/test/test262/unwind builds and to embedders that compile Otter with unwinding enabled. Release binaries built with the current workspace release profile still abort on panic before `catch_unwind` can run.

#### [S5-c] Release panic policy ✅ DONE (2026-04-26)

Switched workspace `[profile.release]` from `panic = "abort"` to `panic = "unwind"`. The S5-b `catch_unwind` wrapper around `RuntimeState::call_host_function` (and the constructor variant in `host_runtime.rs`) is unconditional — with unwinding enabled in release, a panic inside an intrinsic now surfaces as a catchable internal-runtime `TypeError` in production binaries, not just dev/test. Tradeoff: ~2 % binary growth (unwind tables) and a small cold-path overhead at panic boundaries. This matches the V8 / JSC / SpiderMonkey defaults and is required for any embedding use case where the host process must outlive a buggy native binding. Existing `s5_b_native_panic_*` regression tests cover both ordinary call and constructor dispatch; release smoke `JSON.parse("{") + 1n / 0n` catches both errors. Workspace build passes, 1039 lib tests stay green. The `[profile.test262]` override (`inherits = "release"; panic = "unwind"`) becomes a no-op now that release itself unwinds, but is left in place as defence-in-depth in case the release default is ever rolled back.

#### [S6] Thread-local state breaks embedding ✅ DONE (2026-04-24)

Dynamic-import host/registry/referrer + `Math.random` PRNG state moved from `thread_local!` onto `RuntimeState`. Two runtimes on two OS threads run concurrent module graphs without clobbering. `source_compiler/mod.rs` thread-locals audited — transient per-compile, safe.

#### [S7] Event-loop `std::thread::sleep` inside Tokio reactor ⚠️ PARTIAL

S7-a (2026-04-24): `from_current()` path uses `park_timeout` so `RunInterrupt::fire()` unparks immediately. Embedders must wrap in `spawn_blocking` (documented).

#### [S7-b] Fully async `poll_next_async` variant ✅ DONE (2026-04-26)

`OtterRuntime::run_module_async` drives the event loop via `tokio::time::sleep` instead of `std::thread::park_timeout`. Embedders running OtterJS inside an outer tokio reactor (Axum, Tower, tonic) no longer need `tokio::task::spawn_blocking` for timer-heavy scripts — two concurrent runtime instances can multiplex onto a single reactor without one starving the other on a `setTimeout`.

**Architecture choice:** kept the `EventLoopHost` trait sync rather than extending it with `async fn poll_next_async`. Async-fn-in-trait isn't object-safe without a `Box::pin` workaround, and the `&mut dyn EventLoopHost` usage in `run_event_loop` would force every implementor to box every poll. Instead, `OtterRuntime` carries a private `run_event_loop_async` that mirrors the sync drive but yields at timer deadlines. The trait stays the abstraction for plug-in event loops; the async benefit is concentrated in the concrete tokio path where it matters.

**Sleep helper:** `sleep_until_interruptible_async` polls the `RunInterrupt` flag at `MAX_ASYNC_SLEEP_QUANTUM = 50 ms` granularity. `tokio::time::sleep` lacks an unpark-on-flag-set primitive, so we slice the wait — ^C latency stays under 50 ms while the steady-state path keeps the sleep coarse.

**Host-callback wait** stays synchronous (`wait_for_host_callbacks_interruptible` uses a condvar) because the underlying primitive isn't async; the wait is bounded by the next timer deadline so it drains promptly.

**`run_entry_specifier_async` deferred:** the hosted module-graph loader uses `reqwest`'s blocking client + sync FS, so the only async benefit comes from the post-load event loop. Tracked alongside F1 (async fetch).

**Verification:** `run_module_async_drives_timers_under_tokio` test in `runtime::s5_tests` runs two `setTimeout(5)` scripts concurrently on a 2-worker `tokio::Builder::new_multi_thread` and asserts they finish in well under 500 ms — a synchronously-blocking driver inside a single worker would queue them.

`tokio = { version = "1", features = ["time", "rt"] }` added to `otter-runtime`'s deps; `rt-multi-thread`+`macros` are dev-only for the test.

#### [S8] Regex backtracking watchdog ✅ DONE (closed 2026-04-25)

Two-layer ReDoS defence. S8-a (2026-04-24): 1 MB UTF-16 input cap. S8-b (2026-04-25): `ExecConfig { backtrack_limit }` + `ExecError::StepLimitExceeded` added to local `regress` fork (`crates/otter-vm/Cargo.toml:42-45`); otter caps per-exec budget at 10 M steps via `MAX_REGEXP_BACKTRACK_STEPS`; surfaces as catchable `RangeError("…ReDoS protection")`. Bonus: parse-time regex literal validation (`identifiers.rs:500-516`). test262 RegExp 56.2 % → **68.7 %**, 0 timeouts, 0 crashes. Upstream `regress` PR pending — revert `Cargo.toml` once merged.

---

### P1 — Near-term performance & correctness (4–8 weeks)

#### [C1] BigInt represented as decimal string, parsed on every op ✅ DONE (2026-04-25)

`HeapValue::BigInt` now stores `bigint_value::BigIntPayload` (`Inline(i64)` fast path + `Heap(Box<BigInt>)` arbitrary-precision fallback). Arithmetic, bitwise, shift, comparison, equality all run on the structured payload — no decimal-string round-trip per op. Inline ↔ Heap demotion is canonical at construction, so `BigInt === BigInt` reduces to a variant + content check. As a side fix-up the path also closed two adjacent gaps: `Opcode::Negate`/`BitwiseNot` did not handle BigInt (forwarded to `js_to_number`, threw TypeError), and `TestLessThan`/`GreaterThan`/`LessThanOrEqual`/`GreaterThanOrEqual` were int32-only — both now route through `BigIntPayload::neg/bitnot` and `js_abstract_relational_comparison` respectively. 16 unit tests + 11 source-level regression tests. test262 BigInt 40.8 % → **47.4 %** (+10), addition 87.4 % → **91.6 %** (+4), 0 crashes. **C1-b** (NaN-box `TAG_BIGINT_SMALL` to skip the heap shell for inline magnitudes) deferred — would touch every `RegisterValue` site and is gated on a bigger value-encoding sweep.

#### [C2] String concatenation is eager ✅ DONE (2026-04-25; pre-audit landed via the C2 string-hierarchy work tracked in `docs/c2-string-hierarchy-design.md`)

`JsStringRepr` carries `SeqOneByte` / `SeqTwoByte` / `Cons { left, right, depth }` / `Sliced { parent, offset }` / `Thin { forward }`. `concat_strings` short-circuits to a flat seq below `MIN_CONS_LENGTH`, otherwise builds a Cons node with depth = `max(lhs_depth, rhs_depth) + 1`; once depth exceeds `MAX_CONS_DEPTH = 32` the node is force-flattened in place (DOS-safe iterative DFS). `slice_string` produces `Sliced` views, `flatten_string` rewrites the storage in-place to `Thin` so all aliases see the result. `String.prototype.concat`/`slice`/`substring` and the `+` operator (`coercion.rs::js_add`) all dispatch through this layer; equality (`strings_equal`) flattens both sides first. 10 regression tests `c2_*` cover lazy construction, observability via `charCodeAt`, depth-flatten, lone-surrogate preservation, slice-of-slice collapse, and Strict-Eq reflexivity across mixed reprs. Latin-1 auto-detect on input keeps memory at parity with V8 OneByte. Preserved invariant: lone surrogates survive end-to-end (no UTF-8 round-trip)..

#### [C3] RegExp compilation not cached ✅ DONE (2026-04-25)

`HeapValue::RegExp` now carries `compiled: std::cell::OnceCell<regress::Regex>`. `regexp_builtin_exec` borrows the cached engine via `ObjectHeap::regexp_compiled`; first call compiles and caches, subsequent calls reuse. `set_regexp_pattern_flags` (Annex B `RegExp.prototype.compile`) resets the cache. Standalone validate-only `compile_regex` retained for constructor + `compile()` early-error paths. 5 regression tests `c3_regexp_*` cover correctness, lastIndex threading, Annex B reset, SyntaxError on bad pattern, and `.test()` reuse. **Bench (100k `r.exec(s)` over the same literal regex):** 629 ms → 413 ms, **1.52× speedup** end-to-end on the otterjs CLI release binary; the per-call savings dominate as the regex grows. test262 RegExp 83.2 % (paritet — cache is a perf path; correctness already covered). Stretch task "cache UTF-16 encoding of input" not landed — input encoding is the cheap part of `find_from_*` and would need a value-equality key not a handle one.

#### [C4] Feedback vector wiring ✅ DONE (2026-04-24)

Compiler now allocates Comparison/Branch/Call/Property(store) slots at every Test/JumpIf/Call/StaNamedProperty emission site; dispatcher records observations on each lattice. `MAX_JS_STACK_DEPTH` lowered 24→20 (record_* calls bumped per-activation debug frame). 5 regression tests `c4_*`. Follow-up: **C4-b** consume new feedback in JIT (store IC, call IC, comparison specialization) — only producer side closed.

#### [C5] Eval recompiles on every call ✅ DONE (2026-04-25)

`RuntimeState` carries `eval_cache: VecDeque<(EvalCacheKey, Rc<Module>)>` capped at 64 entries with O(n) LRU promote-on-hit (the cache is small enough that the constant beats hashmap chaining). Key is `(DefaultHasher(source), is_field_init)`; field-initializer eval (§B.3.5.2) gets its own slot because the early-error rules diverge. Cache fires for indirect eval only — direct eval inherits the enclosing closure scope, which the compiled `Module` does not capture, so caching it would give wrong results. 3 regression tests `c5_eval_*` cover hit-rate observability via global side-effects, distinct sources sharing the cache, and SyntaxError-on-malformed surviving repeated calls. **Bench (10k indirect-eval calls of one literal source):** 125.7 ms → 108.0 ms, **~14 % speedup** end-to-end on otterjs. The compile pipeline is genuinely cheap relative to the rest of an eval round-trip; the win scales with source size.

#### [C6] Per-call boxing in Activation ✅ DONE (2026-04-25)

`Activation` now uses `Vec<RegisterValue>` + `Vec<Option<ObjectHandle>>` (was `Box<[T]>`) and `RuntimeState` carries two pools (`register_buffer_pool` / `upvalue_buffer_pool`) capped at 64 entries each. `acquire_call_buffers(register_count)` clears + resizes a pooled `Vec` on hit, allocates fresh on miss; `release_call_buffers(...)` returns the buffers cleared but at preserved capacity. Wired through every hot call site: `RuntimeState::construct_callable`, the `CallDirect` dispatch path, the closure call path in `RuntimeState::call_callable`, the generator resume path in `host_runtime::resume_generator_impl`, and the `TailCall` replace-in-place loop in `mod.rs::run_completion_with_runtime_inner`. Activation grew an `into_pooled_buffers(self)` that hands the buffers back; the new `with_pooled_buffers` constructor avoids the per-call zero-fill alloc by accepting pre-acquired vectors. Generator save/resume still copies `registers` to a `Box<[RegisterValue]>` snapshot on each yield (rare path; out of scope to redesign). **Bench (200k closure calls of `f(a,b)`):** 312.5 ms → 286.4 ms, **~8.4 % speedup**. The pool is per-runtime (not thread-local), so the S6 multi-thread embedding contract holds. Stretch task "swap registers field directly into generator save state without `Box<[T]>` snapshot" deferred — would change generator state encoding and is independent of the call-frame win. `#[derive(Clone)]` on `Activation` retained for now; the cascade only fires on the cold generator-resume snapshot path, and removing it would require re-shaping the GeneratorFrame snapshot type — separate refactor.

#### [C7] Hidden-class transitions are monotonic IDs, no transition graph ✅ DONE (2026-04-25)

`ObjectHeap` now carries `shape_transitions: HashMap<(ObjectShapeId, PropertyNameId), ObjectShapeId>`. Every fresh `alloc_object` starts at the canonical `EMPTY_SHAPE_ID = ObjectShapeId(0)`; `set_named_property_storage` looks up `(parent_shape, property)` before deciding to mint a new id. Two `{a:1,b:2}` literals now reach the *same* shape id, so the property IC stays monomorphic across siblings instead of going megamorphic on the 5th distinct allocation. The transition map is unbounded by design — the upper bound on entries is `O(unique-property-sequence-prefixes)` which scales with code-shape complexity, not data volume; pathological code that produces thousands of distinct property sequences is rare. Fallback `allocate_shape` retained for the few non-Object kinds where the transition pattern doesn't apply (closures with auto-installed `name`/`length`, RegExp with prefab `lastIndex`). 3 regression tests `c7_*` cover sibling-shape sharing (verified through behavioural IC stability), property-order disambiguation, and the 1000-sibling bulk pattern. **Bench (100k `{a,b,c}` literals + linear `.a` read):** 4.985 s → 4.706 s, **~5.6 % speedup** end-to-end; the IC saving is bigger than the headline number suggests since most of the bench wall-time is element allocation, not the `.a` read. Macro test262 paritet preserved.

#### [C8] Shape keys + prototype-chain lookup have no cache ✅ DONE (2026-04-25)

`ObjectHeap` now carries `proto_lookup_cache: RefCell<HashMap<(ObjectShapeId, PropertyNameId), ProtoCacheEntry>>` plus a `prototype_generation: u64` counter. `get_property_with_registry` probes the receiver's own shape first (own-property hit returns immediately), then keys the proto cache by `(receiver_shape, property)`. A cache hit validates `entry.generation == prototype_generation` and `owner.shape_id == entry.owner_shape` before reading via `get_shaped`. Misses walk the proto chain and populate the cache with `(owner, owner_shape, slot)`. Invalidation via `prototype_generation += 1` inside `set_prototype` (skipping the no-op early return) — single atomic bump invalidates every cached entry without touching the map. Property delete on the owner mints a fresh shape via the existing `delete_named_property_storage` allocate, so the per-entry shape check picks up slot reshuffles. `RefCell` keeps the cache writable from `&self` accessors which preserves the existing read-API surface in the interpreter. 3 regression tests `c8_*` cover correctness through prototype, `Object.setPrototypeOf` invalidation, and `delete proto.x` invalidation. **Bench (500k method calls through 4-deep `class D extends C extends B extends A`):** 4.642 s → 4.523 s, ~2.6 % speedup; the bench is dominated by the function call (C6 already paid most of that), and the cache hit replaces a 4-hop linear scan with a HashMap probe + 2 shape comparisons. Workloads that read many proto-inherited properties without calling them see a larger fraction of the saving.

#### [C9] String indexed methods re-encode UTF-16 per call ✅ DONE (2026-04-25)

`charAt`/`at`/`codePointAt`/`indexOf`/`lastIndexOf`/`includes`/`startsWith`/`endsWith`/`slice`/`substring`/`padStart`/`padEnd` now read `JsString` WTF-16 backing store directly via `this_js_string_value` + new `arg_js_string_value`. Eliminates per-call `js_to_string` → UTF-8 → `encode_utf16` round-trip and lone-surrogate U+FFFD mangling. Fixes spec correctness for `charAt`/`at` — they now index by UTF-16 unit, not Unicode code point (`"😀".charAt(0)` returns `"\uD83D"` per §22.1.3.2). 6 regression tests `c9_*`.

#### [C10] `String.prototype.normalize` silently returns input ✅ DONE (2026-04-24)

`string_normalize()` dispatches to `unicode_normalization::UnicodeNormalization::{nfc,nfd,nfkc,nfkd}`, throws `RangeError` for invalid form. 4 regression tests `c10_*`.

#### [C11] `Number.prototype.toString(radix)` falls to exponential for large N ✅ DONE (2026-04-24)

Ported V8's `DoubleToRadixCString` (integer part via `floor(n/radix)`, fractional part via delta-bounded repeated multiply with round-up carry). 5 regression tests `c11_*`.

#### [C12] BigInt from large Number silently truncates ✅ DONE (2026-04-24)

Bit-exact IEEE754 → BigInt via `(mantissa | (1<<52)) << adjusted_exponent`, sign applied last. Preserves `Math.pow(2, 70)`. 6 regression tests `c12_*`. C1 (BigInt op-side string round-trip) unchanged.

#### [C13] RegExp captures lossy UTF-16 ✅ DONE (2026-04-24)

All 3 capture output sites in `regexp_builtin_exec` build `JsString::from_utf16(&[u16])`. Adjacent: `charCodeAt` reads WTF-16 directly; `String.fromCharCode` preserves lone surrogates. 3 regression tests `c13_*`. Caveat: regex *input* still goes through `js_to_string` UTF-8 — follow-up **C13-b** for end-to-end WTF-16 haystacks.

#### [C14 / C-args] Arguments object missing entirely on the new VM stack ✅ DONE (2026-04-25)

Audit found `arguments` was completely absent from the new VM stack (lost in old-VM merge); MEMORY.md's old test262 numbers reflected the parked stack. Implemented unmapped Arguments exotic object end-to-end (§10.4.4.7): new `Activation::argc` field, dispatch handler for `CreateArguments`, source-compiler `emit_arguments_object` with V8-style "argumentsObjectNeeded" elision via oxc_ast_visit `ArgumentsUseScanner`. Bonus: `%ArrayIteratorPrototype%.next` falls back to `LengthOfArrayLike` + `OrdinaryGet` for non-Array iterables; `SpreadIntoArray` falls back to protocol path. 6 regression tests `c_args_*`. test262 `language/arguments-object` 0 % → **46.7 %**, RegExp 68.7 % → **70.9 %**. Caveat: mapped variant (sloppy + simple params, UpvalueCell aliasing) deferred as **C-args-b**.

---

### P1 Observability

#### [O1] Chrome DevTools Protocol server (`--inspect`) ☐ DEFERRED

L-effort task — full CDP requires WebSocket framing + handshake, the `/json` discovery endpoint, per-session message routing, and four protocol domains (`Runtime`, `Debugger`, `Profiler`, `HeapProfiler`). The on-disk profile/snapshot writers landed via O2/O3/O4 share their data shape with the corresponding CDP responses, so a future inspector crate can serialise CpuProfile → `Profiler.stop` and HeapInfoSnapshot → `HeapProfiler.takeHeapSnapshot` without reformatting. The task is unblocked but not started — it remains in P1 Observability for the next milestone.

#### [O2] .heapsnapshot walker over the live heap ✅ DONE (2026-04-26)

`ObjectHeap::heap_snapshot_info` aggregates the live slot table into a new `HeapInfoSnapshot` (object_count, tracked_bytes, per-type count+size). `OtterRuntime::take_heap_snapshot()` returns a Chrome-DevTools `.heapsnapshot` JSON value built via `otter_profiler::MemoryProfiler::to_heapsnapshot`; `OtterRuntime::enable_heap_snapshot(path)` wires the at-exit flush. CLI flags `--heap-snapshot` / `--heap-snapshot-file=PATH` expose it. Reuses the existing `collect_type_stats` walker (one slot-table scan shared with the test262 leak-profile path) — no new GC iteration API needed once `for_each` was already in place. Regression test `heap_snapshot_writes_chrome_devtools_format_on_drop` validates the file is non-empty and carries the load-bearing `snapshot.meta.node_fields` schema field.

#### [O3] Wire CPU + async-trace profilers to CLI flags (ship what's built) ✅ DONE (2026-04-26)

Three wire-points landed:

1. **`RuntimeState::sample_hook`** — `Option<Arc<dyn Fn(&[StackFrameInfo]) + Send + Sync>>` field on the VM. `poll_back_edge` (already on the back-edge hot path) fires the hook with the live shadow stack when set; cost when unset is one null-pointer check.
2. **`OtterRuntime::install_cpu_profiler(profiler, interval, output_path, folded_path)`** — captures `Arc<CpuProfiler>` + interval-gated `Mutex<Instant>` in a hook closure that translates `StackFrameInfo` to `otter_profiler::StackFrame` (with line/column resolved through each module's source map). On drop, the runtime stops the profiler, writes a V8 `.cpuprofile` JSON, and renders a perf-folded `.folded` file (semicolon-joined stacks + hit count, sorted).
3. **`OtterRuntime::install_async_tracer`** — passive sink for `AsyncTracer`; future timer/microtask/`fetch` host sites push spans, the runtime serialises a Chrome-trace JSON on drop.

CLI flags `--cpu-prof[=stem]` / `--cpu-prof-interval=us` / `--cpu-prof-dir=PATH` / `--async-trace[=path]` now do something — they were declared-but-not-hooked before. Smoke test on a busy interpreter loop produces a valid `.cpuprofile` with non-zero samples + a `<top-level>;busyLoop N` folded line.

**Caveats:**
1. Sampling fires only on the interpreter back-edge poll, so JIT-compiled hot loops (which bypass `poll_back_edge`) currently do not contribute samples. The plan's "ship what's built" framing scopes the win to interpreter-resident work; full JIT sampling is a separate refactor that needs a sampling-thread + signal-driven (SIGPROF) primitive.
2. The `cpu_profiler_writes_files_on_drop` regression test in `otter-runtime` is `#[ignore]` pending follow-up: under the release-build cargo-test harness the test binary blocks in `UE` state for the duration of `loop_n(50000)`, suggesting the hook fires from inside a JIT-OSR'd path where back-edge counter and sample closure share re-entrancy. The CLI flag itself was smoke-tested manually (`--cpu-prof` on `busyLoop`) and produces valid `.cpuprofile` + non-empty `.folded` outputs, so this is a test-harness issue, not a runtime regression.

#### [O4] Structured error object with V8 extensions ✅ DONE (2026-04-26)

Already implemented in `crates/otter-vm/src/intrinsics/error_class.rs`: `Error.captureStackTrace(target, constructorOpt?)` is installed as a V8-extension static method on the base `Error` constructor, and `err.stack` is a lazy accessor that defers `format_v8_stack` until first read. `capture_error_stack` walks the runtime's shadow `frame_info_stack`, source-map-resolves each frame's PC into `(line, column)`, and produces V8-format `<Name>: <message>\n    at <fn> (<url>:<line>:<col>)` output. The `constructorOpt` parameter is honoured — frames at or above the innermost matching closure are skipped. Four regression tests `o4_*` verify: format header, constructor skip, default capture-from-caller, non-object target throws TypeError.

---

### P2 — Feature completeness (3–6 months)

#### [F1] `fetch` / `http` / `https` go async
**Files:** `crates/otter-web/src/request_response_api.rs` (whole file), `crates/otter-vm/src/event_loop.rs`.
**Problem:** `perform_fetch` is sync. Any HTTP work blocks the event loop.
**Proposal:** Redesign `fetch` around a future-returning host callback. When JS calls `fetch(url)`, allocate a `Promise`, schedule a `tokio::spawn` on the host runtime, resolve/reject from the background task via the microtask channel. Use `reqwest`'s async client; retire the blocking one.
**Effort:** L (touches request/response/body/headers wiring).
**Success metric:** 100 concurrent fetches complete without pausing timer callbacks.

#### [F2] `node:fs`, `node:path`, `node:url`, `node:buffer` — real, not stubs
**Files:** `crates/otter-nodejs/src/fs.rs` (463 LOC currently stubs), `path.rs` (255 LOC, incomplete), new `buffer.rs`.
**Problem:** The top-1000 npm packages all assume these. Current stubs are enough for `assert` and `util` only.
**Proposal:** Match Node's surface for `fs.readFile/writeFile/promises/createReadStream/createWriteStream`, `path.join/normalize/parse`, Buffer as `Uint8Array` subclass with Node methods (`toString('utf-8')`, `from`, `alloc`, slice).
**Effort:** XL overall; parallelisable by module.
**Dependencies:** F1 (async I/O) for streams.

#### [F3] WHATWG Streams (ReadableStream / WritableStream / TransformStream)
**Files:** new `crates/otter-web/src/streams.rs`.
**Problem:** Partial ReadableStream stub in `request_response_api.rs:51-66`; no WritableStream, no TransformStream.
**Proposal:** Full WHATWG Streams spec impl. Integrate with F1 so `fetch().body` is a real ReadableStream.
**Effort:** L.

#### [F4] WebCrypto `crypto.subtle`
**Files:** new `crates/otter-web/src/webcrypto.rs`.
**Problem:** Absent.
**Proposal:** RustCrypto + `ring` for AES/RSA/ECDSA/ECDH/HMAC/SHA-*/HKDF/PBKDF2.
**Effort:** L.

#### [F5] AbortController / AbortSignal
**Files:** new `crates/otter-web/src/abort.rs`, integrations in fetch, timers, streams.
**Problem:** Missing entirely.
**Effort:** M.

#### [F6] WebSocket client + server
**Files:** new `crates/otter-web/src/websocket.rs`.
**Effort:** M (client) + M (server).

#### [F7] Worker threads (Track W6 / N9)
**Files:** new `crates/otter-vm/src/worker.rs`; ties to §2.7 thread-local cleanup.
**Problem:** Single-threaded. Not a proper runtime without.
**Proposal:** One OS thread per worker; each worker owns its own `OtterRuntime`. `postMessage` via structured-clone + crossbeam channel. Depends on eliminating thread-local globals (S6).
**Effort:** XL.
**Dependencies:** S6 (thread-local cleanup), O1 (CDP should multiplex workers).

#### [F8] CommonJS `require` interop
**Files:** `crates/otter-runtime/src/host/module_loader.rs`, `crates/otter-nodejs/src/module_registry.rs`.
**Problem:** ESM-only. Node ecosystem is 50/50 CJS.
**Proposal:** Recognise `.cjs`, `"type":"commonjs"` in package.json, `require()` fn exposed to CJS modules, `module.exports` / `exports` wrapper.
**Effort:** L.

#### [F9] REPL
**Files:** `crates/otterjs/src/commands/repl.rs` (new), `crates/otter-vm/src/interpreter/runtime_state/eval.rs`.
**Problem:** Disabled (`main.rs:301`). Table-stakes DX.
**Proposal:** Line editor via `rustyline`, multi-line detection via oxc parser ("incomplete"), scope-inspect completion via shapes.
**Effort:** M. Depends on C5 (eval cache).

#### [F10] Test runner (`otter test`)
**Files:** `crates/otterjs/src/commands/test.rs` (new).
**Problem:** Disabled (`main.rs:302`).
**Proposal:** Jest-compatible surface, snapshot testing, coverage (needs source-map + instruction counter from profiler).
**Effort:** L. Depends on F2, F9.

---

### P3 — Strategic architectural bets

#### [A1] Close the GC fork: commit to generational or abandon it
**Files:** entire `crates/otter-gc`, `crates/otter-vm/src/object.rs` allocation sites.
**Problem:** Two GCs, wrong one wired. Either `TypedHeap` becomes the real GC (and `GcHeap` / `scavenger.rs` / `barrier.rs` is deleted) or `TypedHeap` is retired and VM allocations go through `GcHeap::alloc_young` + write-barrier-instrumented property sets.
**Decision criterion:** If target is "production JS runtime", only `GcHeap` gets to pause times <10 ms at 1 GB. Choose (b).
**Proposal (b):** Phased migration:
  1. Make `GcHeap::alloc_young` callable from `otter-vm/src/object.rs::alloc_heap_value`. Gate under a feature flag.
  2. Replace `Handle(u32)` (slot-index) with `GcRef<HeapValue>` (page-relative pointer, V8-style Local/HandleScope on top).
  3. Introduce `write_barrier(container, field, new_value)` at every property-store site. Audit with a grep that fails CI if `ObjectCell::borrow_mut` writes without a barrier.
  4. Turn on incremental marking: wire `GcHeap::drain_with_budget` into the interpreter back-edge poll.
  5. Turn on scavenger: gate young→old promotion on survival count (one cycle).
  6. Delete `TypedHeap` and related compatibility shims.
**Effort:** XL (quarter+).
**Risks:** Write-barrier bug class is unforgiving — missing barrier = UaF the instant concurrent marking goes on. Build a sanitizer (dev-mode tagged-pointer verification) up front.
**Success metric:** 1 GB heap, P99 GC pause ≤ 10 ms under allocation rate of 500 MB/s.

#### [A2] JIT tier 2 (Maglev-class) or retire baseline
**Files:** `crates/otter-jit/**`, new `crates/otter-jit-optimizer/`.
**Problem:** 20 % opcode coverage means JIT contributes almost nothing to real workloads. Either build a proper tier-2 that handles property access, calls, and allocations (with deopt materialisation), or shut down baseline and spend the budget on interpreter throughput.
**Decision criterion:** If the runtime is for general JS, invest in tier 2. If it's for FHIR ETL numeric loops, baseline is enough — invest elsewhere.
**Proposal (tier 2):**
  - SSA-based IR over bytecode (turboshaft-style: single graph for all functions inlined).
  - Speculative types from feedback vector (C4).
  - Property-IC-in-code (load guard → patched stub or inline slot address).
  - Deopt via side table: maps JIT PC → bytecode PC + register map; reconstruct interpreter frame on bail.
  - Allocation sinking for object literals in loops.
**Effort:** XL (quarter+).
**Dependencies:** C4 (feedback), C7 (shape transitions), A1 (GC with write barriers; else JIT can't safely mutate heap).

#### [A3] Inspector-driven snapshots / isolate warm-start
**Files:** new `crates/otter-snapshot/`.
**Problem:** ~10–100 ms per-runtime startup (intrinsics boot). Serverless is off the table.
**Proposal:** V8-style snapshot: after intrinsics installation, serialise the `TypedHeap` / `GcHeap` slot table + string table to a binary blob. At startup, mmap and relocate. Shrinks cold start to <5 ms.
**Effort:** L.
**Dependencies:** A1 (stable heap layout).

#### [A4] Multi-isolate per process
**Files:** `crates/otter-runtime/src/runtime.rs`, everything thread-local.
**Problem:** Required for X2 (multi-tenant).
**Proposal:** Each isolate owns its `TypedHeap` / `GcHeap` and Realm. Shared nothing. Worker threads (F7) become a first-class version of this.
**Effort:** XL.
**Dependencies:** S6, F7.

#### [A5] Drop parked crates from active build graph OR delete them
**Files:** `crates/otter-node-compat/`, reconsider `crates/otter-nodejs`.
**Problem:** `CLAUDE.md` says both are parked; reality is only `otter-node-compat` is parked, `otter-nodejs` is **live** and shipped by the CLI. Documentation lies.
**Proposal:** Either
  (a) Delete `otter-node-compat` (it's 2 files; the test262-style Node harness pattern can live in `crates/otter-test262/` style instead), or
  (b) Rename the two crates to match reality: `otter-nodejs` is active (confirmed by `main.rs:504`), `otter-node-compat` is a test harness. Update `CLAUDE.md` + `AGENTS.md` + ROADMAP to match.
**Effort:** XS.

---

## 9. Milestones

### Milestone 1 — Stability baseline (4 weeks)

**Exit criteria:**
- All P0 tasks closed (S1–S8).
- `just test262` previously-hanging tests re-enabled; no infinite-loop tests in `known_panics`.
- Ctrl-C returns to shell in <100 ms.
- Release build on macOS ARM64 passes JIT smoke test.
- Multi-threaded Axum integration test runs without panicking.

**Task list:** S1, S2, S3, S4, S5, S6, S7, S8, A5.

**Effort:** ~12 person-weeks (heroic pace for one engineer; parallelisable).

**Risks:**
- S4 (MAP_JIT) may require downstream release-binary signing changes. Document in `docs/deployment.md`.
- S3 is a large mechanical refactor; review in small slices.

### Milestone 2 — Correctness & performance parity with Node.js (first cut) (8 weeks)

**Exit criteria:**
- test262 pass rate on `language/**` ≥ 95 %, `built-ins/**` ≥ 90 %.
- Arithmetic / string / BigInt microbenchmark performance within 3× of Node.js on sunspider + octane derivatives.
- Feedback vector fully wired (C4) — IC hit/miss counters non-zero across all tracked sites.

**Task list:** C1–C14. Prioritise in order: C4 (unblocks perf work) → C7 (hidden-class transitions) → C6 (no-malloc calls) → C2 (rope strings) → C8 (proto cache) → C9 (string indexing) → C3 (regex cache) → C5 (eval cache) → C10–C13 (correctness).

**Effort:** ~20 person-weeks.

**Risks:**
- C7 (shape transitions) interacts with GC — when A1 happens later, shape cloning has to coexist with write barriers. Plan both designs together.
- C6 uses thread-local free lists — confirm no collision with S6 design.

### Milestone 3 — Observability & DX (12 weeks)

**Exit criteria:**
- `otter --inspect script.ts` attachable from Chrome DevTools / VS Code JS Debug.
- `.heapsnapshot` files open in Chrome DevTools.
- REPL and `otter test` work.
- Benchmark harness in `benches/*.rs` compares against Node / Bun / Deno.

**Task list:** O1, O2, O3, O4, F9, F10, plus perf benchmarks.

**Effort:** ~14 person-weeks.

**Risks:**
- CDP protocol surface is large. Ship minimum viable: Debugger + Runtime + Profiler + HeapProfiler only.

### Milestone 4 — Feature & ecosystem parity (quarter+)

**Exit criteria:**
- `fetch` non-blocking, `http` + `https` + `net` real (F1, F2).
- WHATWG Streams (F3).
- WebCrypto (F4).
- AbortController (F5).
- WebSocket (F6).
- Worker threads (F7).
- CommonJS interop (F8).
- Top-1000 npm packages smoke test passes for top 100.

**Task list:** F1–F8.

**Effort:** ~ 40+ person-weeks.

**Risks:**
- F7 blocked on A1 + A4. If those aren't done first, workers will race on shared heap state.

### Milestone 5 — Strategic bets (as capacity allows)

**Exit criteria per bet:**
- A1: P99 GC pause < 10 ms at 1 GB live heap, allocation rate 500 MB/s.
- A2: tier-2 JIT produces correct + fast code on spidermonkey kraken / sunspider subset.
- A3: cold start < 5 ms on `otter -e "console.log('ok')"`.
- A4: multi-tenant demo runs 100 isolates in one process without leak.

**Effort:** XL each; budget at least one quarter per bet.

---

## 10. Unknowns / open questions for the team

1. **GC direction.** `ROADMAP.md` Track G (G1–G10) describes the page-based generational GC as unstarted. `otter-gc` has a near-complete implementation **that isn't wired**. Is the plan to finish wiring, or rewrite? If rewriting, retire the dead code now.
2. **Bytecode V2.** `docs/bytecode-v2.md` exists; `bytecode_v2` feature gate is off by default. Is the current `bytecode/` the one tier 2 JIT targets? Or is V2 going to land and obsolete baseline? Plan must be coherent.
3. **`otter-nodejs` vs. `otter-node-compat`.** Resolve the parked-vs-active docs lie (A5).
4. **JIT tier 2 scope.** Is the FHIR use case stable enough to make int32-arithmetic-loop baseline "good enough"? If yes, stop investing in JIT features and put cycles on interp throughput + GC.
5. **Security posture.** Is the target to ship a hardened runtime suitable for running untrusted code (LLM sandboxes — Track X3)? If yes, F7 (worker isolates) + A4 (multi-isolate) become table stakes, not strategic bets.
6. **Test262 target.** ROADMAP claims 95 % as the M10 target. Current `ES_CONFORMANCE.md` is empty; present-day baseline per the prompt: ~81 % on compound-assignment, ~100 % on comma, ~82 % on typeof. Is there a canonical report file being generated, or does the prompt's number come from a run snapshot?

---

## Appendix A — File reference map

Hot spots that come up repeatedly in the plan:

- `crates/otter-gc/src/typed.rs:233-262` — `alloc()` (returns unconditionally after OOM flag set).
- `crates/otter-gc/src/typed.rs:378-387` — `sweep_phase` (full scan).
- `crates/otter-gc/src/typed.rs:391-399` — `maybe_collect` (full STW entry).
- `crates/otter-gc/src/heap.rs:192-226` — `enforce_heap_limit` (real implementation — unused).
- `crates/otter-gc/src/scavenger.rs:75-190` — scavenger implementation — unused.
- `crates/otter-gc/src/barrier.rs:84-164` — write barriers — no call sites.
- `crates/otter-vm/src/value.rs:28-44,94-116` — NaN-box encoding.
- `crates/otter-vm/src/object.rs:900-920,8028-8032,6091-6108,1845-1896` — property storage, shape alloc, proto walk, heap.
- `crates/otter-vm/src/js_string.rs:25,305-311,364-368,383-387` — string repr, concat, equality, hash.
- `crates/otter-vm/src/interpreter/mod.rs:159,1423-1429` — interrupt flag + back-edge poll.
- `crates/otter-vm/src/interpreter/dispatch.rs:57-2909,1743-1752` — dispatch, spread-into-array.
- `crates/otter-vm/src/interpreter/runtime_state/mod.rs:649-703` — own-key enumeration.
- `crates/otter-vm/src/interpreter/activation.rs:55-101,139-141` — per-call allocations.
- `crates/otter-vm/src/interpreter/frame_runtime.rs:67-141` — feedback recording (all `#[allow(dead_code)]`).
- `crates/otter-vm/src/interpreter/runtime_state/eval.rs:40` — uncached eval compile.
- `crates/otter-vm/src/intrinsics/string_class.rs:463-560,1328-1337` — string indexing + normalize.
- `crates/otter-vm/src/intrinsics/regexp_class.rs:397-453,497-529` — regex compile, lossy captures.
- `crates/otter-vm/src/intrinsics/bigint_class.rs:253-313,362-388` — string-based BigInt.
- `crates/otter-vm/src/intrinsics/number_class.rs:409-442` — toString(radix) fallback.
- `crates/otter-vm/src/module_loader.rs:27-53` — thread-local dynamic-import state.
- `crates/otter-vm/src/event_loop.rs:274-393,62-237` — tokio driver, timer heap.
- `crates/otter-vm/src/promise.rs:220-268` — promise resolution.
- `crates/otter-vm/src/microtask.rs:65-75,112-124` — three-queue drain.
- `crates/otter-runtime/src/runtime.rs:94-116,267-276,578-613` — runtime config, timeout guard, event loop.
- `crates/otter-runtime/src/host/module_loader.rs:1-150` — oxc-based resolution, LRU source cache.
- `crates/otter-web/src/request_response_api.rs:12,292` — blocking fetch, 30 s timeout.
- `crates/otter-jit/src/code_memory.rs:33-95,96-100,126-137` — mmap/mprotect, no MAP_JIT, partial I-cache flush.
- `crates/otter-jit/src/baseline/mod.rs:212-225,819-843,3670-3671` — supported opcodes, OSR filter, misleading MAP_JIT comment.
- `crates/otter-jit/src/tier_up_hook.rs:56-96,174-204` — bailout demotion, blacklist threshold.
- `crates/otter-jit/src/code_cache.rs:48-56,195-203` — unenforced size limit, no GC integration.
- `crates/otter-modules/src/ffi.rs:147-195,254-255` — libffi boundaries, Send+Sync assertions.
- `crates/otterjs/src/main.rs:292-350,504` — CLI command routing, extension registration.

---

## Appendix B — What to do Monday morning

If you have one engineer-week and want the highest-leverage single change:

1. **Write a failing integration test** that spins up `OtterRuntime`, calls `function f(){f()}; f();` from script, and asserts the runtime returns a `RangeError` within 1 s.
2. Watch it `SIGSEGV`.
3. Implement S2 (stack-overflow protection) — increment `call_depth`, throw on exceed.
4. Remove `RegExp/property-escapes/generated` from `known_panics`, add an equivalent test for the regex path and fix S8 in the same spirit.
5. Ship. One commit, one day, two whole-process-crash classes closed.

Then you have earned the right to argue about milestone 2.

---

## Appendix C — Work log

- **2026-04-23 — S2** stack overflow protection: `MAX_JS_STACK_DEPTH=24` guard + catchable RangeError. 4 tests `s2_*`.
- **2026-04-23 — S5-a** signal handling: process-wide registry + `signal_shutdown()`; CLI ^C/SIGTERM. 4 tests.
- **2026-04-23 — S1-a** watchdog (first wave): `check_interrupt_interp()` + SpreadIntoArray, Map/Set ctor, Object.assign. 2 tests.
- **2026-04-23 — S3-a** back-edge OOM + interrupt poll via `poll_back_edge()`. 2 tests. Found latent bug: `gc_safepoint` defined but never called.
- **2026-04-24 — S6** thread-local cleanup: dynamic-import + `Math.random` moved onto `RuntimeState`. 2 tests.
- **2026-04-24 — S8-a** regex 1 MB UTF-16 input cap. 2 tests.
- **2026-04-24 — S7-a** `park_timeout` swap so `RunInterrupt::fire()` unparks. 1 test.
- **2026-04-24 — S4** MAP_JIT + `pthread_jit_write_protect_np` on macOS ARM64; release signing workflows. 3 tests.
- **2026-04-24 — S5-b** `catch_unwind` on native descriptor dispatch (unwind builds). 2 tests.
- **2026-04-24 — S1-b** shared `NATIVE_LOOP_POLL_INTERVAL=4096`: proxy traps, yield* helpers, Reflect.ownKeys, TypedArray loops. 2 tests.
- **2026-04-24 — S3-b** `TypedHeap::alloc()` → `Result`; threaded through VM/runtime call paths.
- **2026-04-24 — `source_compiler/mod.rs` split** (C4 follow-up): 12 765 → 1 745 lines via 11 sibling modules. Pure mechanical extraction.
- **2026-04-24 — C4** feedback vector wiring: Comparison/Branch/Call/Property(store) slots populated. `MAX_JS_STACK_DEPTH` 24→20. 5 tests `c4_*`. Consumer side carved as **C4-b**.
- **2026-04-24 — C10/C11/C12/C13** spec-correctness bundle: normalize via unicode-normalization, toString(radix) via V8 `DoubleToRadixCString`, BigInt(Number) bit-exact IEEE754, regex captures preserve WTF-16. 18 tests.
- **2026-04-24 — workspace build repair** (S3-b follow-up): threaded `Result` through `otter-nodejs`/`otter-web`/`otter-modules`/`otter-test262`.
- **2026-04-25 — eager regex literal parse-validation** (S8-b follow-on): `RegExpLiteral` → `regress::Regex::with_flags` at compile time. test262 RegExp 56.2 % → 68.7 % (+344).
- **2026-04-25 — S8-b** regex step-limit (cross-repo): added `ExecConfig { backtrack_limit }` + `ExecError::StepLimitExceeded` to local `regress` fork; otter caps at 10 M steps. 3 tests `s8_b_*`. Upstream PR pending — revert `Cargo.toml` once merged.
- **2026-04-25 — C14 / C-args** Arguments object end-to-end: `Activation::argc` + `CreateArguments` dispatch + `emit_arguments_object` with V8-style elision (oxc_ast_visit `ArgumentsUseScanner`). Bonus: generic `%ArrayIteratorPrototype%.next` for non-Array iterables; `SpreadIntoArray` protocol fallback. 6 tests `c_args_*`. test262 RegExp 68.7 % → **70.9 %**, `language/arguments-object` 0 % → **46.7 %**. Caveat: mapped variant carved as **C-args-b**.
- **2026-04-25 — plan file cleanup**: collapsed closed-task post-mortems in §8 to 1-2 line summaries (1087 → ~620 lines), removed duplicate C11/C12/C13 stubs.
- **2026-04-25 — C9** WTF-16-preserving string indexing: 12 `String.prototype` methods (`charAt`/`at`/`codePointAt`/`indexOf`/`lastIndexOf`/`includes`/`startsWith`/`endsWith`/`slice`/`substring`/`padStart`/`padEnd`) now read `JsString` WTF-16 storage directly via `this_js_string_value` + new `arg_js_string_value`. Eliminates per-call UTF-8 round-trip + fixes spec correctness for `charAt`/`at` (UTF-16 code-unit indexing, not Unicode code-point). 6 tests `c9_*`. test262 String/prototype 68.8 % overall (`indexOf` 74.5 %, `slice` 71.1 %, `charAt` 70.0 %).
