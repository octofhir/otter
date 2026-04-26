# Otter New Engine Foundation Plan

This document defines the first production-grade steps for rebuilding Otter's
JavaScript/TypeScript engine foundation. The goal is not to patch the current
runtime into shape piece by piece. The goal is to establish a small, correct,
measurable VM and interpreter core that can grow into a competitive runtime
without carrying avoidable architectural debt.

The old implementation remains in the repository for reference and compatibility
while this plan starts. Do not delete it as part of the foundation work. Also do
not create a second long-lived product runtime stack in the active build graph.
The new engine work must either replace active crate internals in controlled
slices or live behind explicit, temporary, non-shipping gates until the slice is
ready to become the active path.

New foundation crates may start in a staging directory such as
`crates-next/`, `engine-next/`, or another clearly named temporary home. That
directory is allowed so the new core can stay clean while old code remains for
reference. It must have a written promotion rule: once the foundation slice is
accepted, the staged crates are renamed/moved into the canonical active crate
names and the old path is parked or deleted. Staging is a workspace hygiene tool,
not permission to keep two production engines.

## North Star

Otter must become a production JavaScript/TypeScript runtime written in idiomatic
Rust:

- Correct ECMAScript semantics are non-negotiable.
- TypeScript is supported out of the box from the first public runtime slice:
  `.ts` input is a first-class path, not a plugin or post-MVP loader trick.
- The interpreter must be designed for performance from day one.
- Runtime safety must be enforced by types, capabilities, heap limits, and
  explicit interrupt points.
- Every representation decision must have one owner and one canonical storage
  path.
- Benchmarks and conformance tests are part of the feature, not follow-up work.
- The public Rust API and CLI are first-class product surfaces from the start,
  not wrappers added after the VM is "done".

The first milestone is intentionally narrow: a high-quality bytecode VM,
interpreter, value model, string model, object model, call frame model, minimal
compiler pipeline, TypeScript stripping/lowering path, thin public runtime API,
and CLI commands for running, printing, checking, testing, and profiling a small
but real JavaScript/TypeScript subset. Web APIs, Node.js compatibility, package
management, FFI, and JIT tiers wait until the interpreter foundation can support
them without hacks.

## Non-Negotiable Architecture Rules

1. **No half migrations.** A runtime type has one representation at a time. No
   dual string heaps, dual object stores, or permanent bridge functions between
   old and new tags.
2. **No wrapper allocation on primitive hot paths.** Primitive receiver property
   lookup walks the prototype chain with the primitive as receiver. Allocate
   wrapper objects only for escaping operations such as `Object("x")` or `with`.
3. **Strings are not UTF-8 internally.** Store JS strings as WTF-16 from the
   start, with a planned Latin-1/WTF-16 hybrid. Never round-trip through UTF-8
   for indexed string operations, slicing, comparison, regexp capture, or
   `charCodeAt`.
4. **Ropes from the first string concat milestone.** String concatenation must
   support `Cons`, `Sliced`, and `Thin` representations with lazy flattening.
   Do not land an eager-concat-only implementation for general `+`.
5. **Deterministic order where the spec requires it.** Object property order,
   `Object.keys`, `JSON.stringify`, maps, sets, and iterators use ordered
   storage or explicit ordered views. No `HashMap` in observable iteration
   paths.
6. **No unbounded recursion.** JSON, regexp, object traversal, AST traversal,
   tracing, formatting, and debug serialization use explicit stacks or hard
   depth caps.
7. **Unsafe is isolated.** `#![forbid(unsafe_code)]` belongs in every crate
   except `otter-gc` and `otter-jit`. Allowed unsafe code requires local
   `// SAFETY:` rationale and tests covering the invariant.
8. **No per-call heap churn by default.** Calls, argument passing, pending job
   queues, and temporary buffers use inline storage (`SmallVec`) or reusable
   runtime-owned buffers.
9. **Heap caps are hard.** Allocation checks happen before mutation. If the
   allocation cannot fit, return `OutOfMemory` and leave the heap unchanged.
10. **Native loops are interruptible.** Any loop over user-controlled data polls
    an interrupt/checkpoint every 4096 iterations by default.
11. **JIT platform rules are designed up front.** macOS uses `MAP_JIT`,
    `pthread_jit_write_protect_np`, and release entitlements before JIT work is
    considered shippable.
12. **No `println!` debugging in engine crates.** Use `tracing` with structured
    fields and `RUST_LOG` controls. CLI user output is the only exception.
13. **Public APIs expose product concepts, not VM guts.** Embedders should see
    `Runtime`, `RuntimeBuilder`, `Script`, `Module`, `ExecutionResult`,
    `Diagnostic`, `CapabilitySet`, and resource limits. Raw bytecode, heap
    handles, object shapes, and frame internals stay behind unstable/internal
    modules until explicitly promoted.
14. **CLI commands are thin and testable.** `otter run`, direct-file shorthand,
    `otter eval`, `otter test`, `otter check`, and diagnostic/profiling flags
    should all call the same public runtime API that embedders use.
15. **Interpreter first, JIT later.** No JIT implementation work lands during
    foundation. The VM may preserve metadata useful for a future JIT, but all
    execution, testing, profiling, and performance targets in this phase are
    interpreter-only.
16. **TypeScript first, not TypeScript later.** `.ts` parsing, type syntax
    stripping/lowering, source spans, sourcemap-ready diagnostics, and CLI
    execution are part of the foundation path.
17. **OXC owns parsing.** JavaScript/TypeScript parsing, syntax errors, spans,
    and AST traversal use OXC libraries. Do not write a custom parser or regex
    parser for JS/TS syntax.
18. **Debuggability is foundation work.** Bytecode dump, disassembly, source
    spans, structured diagnostics, and trace hooks land with the VM harness, not
    after bugs become hard to find.

## Foundation Shape

The first engine slice is deliberately small:

- Parser: `oxc`, no regex parsing for JavaScript or TypeScript.
- TypeScript frontend: `.ts` files parse out of the box; type-only syntax is
  stripped/lowered before bytecode emission while preserving source spans.
- Compiler: AST to Otter bytecode with TypeScript source spans preserved.
- VM: register or accumulator bytecode VM chosen by measurement, not taste.
- Interpreter: tight dispatch loop, clear fast paths, no hidden allocation.
- Values: immediate small integers, doubles, booleans, null/undefined, GC
  references, symbols, BigInts, and strings with stable tag semantics.
- Objects: hidden classes/shapes, ordered property storage, prototype chain,
  property descriptors, and explicit receiver handling.
- Strings: WTF-16 backing, rope variants, substring/slice views, optional future
  Latin-1 specialization.
- Calls: compact frame layout, inline small argument storage, no automatic
  `arguments` object materialization unless required.
- GC boundary: all object/string/function allocation goes through one fallible
  allocator API with rooting rules documented and tested.
- Interrupt/OOM boundary: every bytecode back-edge and native loop checkpoint
  calls the same runtime checkpoint helper.
- Public API: a stable builder, capability model, script/module execution
  methods, structured diagnostics, timeout/heap controls, and explicit result
  types.
- CLI: direct script execution, eval/print, `otter test` as the engine test
  harness, check-only compile/type pipeline hook, trace/profiling switches, and
  machine-readable output modes for CI.
- Debugging: bytecode dump, readable disassembly, source-span annotated
  instruction traces, and structured diagnostics are available from the first VM
  harness milestone.

## VM and Interpreter Design Bar

The VM and interpreter are the product core. They must be designed as if a
debugger, profiler, heap snapshotter, Test262 harness, TypeScript runtime path,
and production embedder will all depend on them tomorrow. "Simple first version"
means small surface area, not sloppy architecture.

VM requirements:

- Bytecode format is compact, versioned, disassemblable, and stable enough for
  tests and traces.
- Every instruction has a clear stack/register effect, operand encoding, source
  span policy, interrupt behavior, and allocation behavior.
- The compiler emits feedback slots for operations that will need inline caches:
  property loads/stores, calls, arithmetic, comparisons, branches, element
  access, and iterator protocol operations.
- Bytecode objects carry enough metadata for stack traces, source maps,
  profiling, TypeScript diagnostics, and future code caching.
- Exception handling, lexical environments, closures, and generators are
  designed up front even if not fully implemented in M1.
- Debug/disassembly output is machine-readable and snapshot-testable.
- `otter --dump-bytecode`, `otter --dump-bytecode=json`, or equivalent CLI
  controls exist early enough that every new instruction can be inspected from a
  fixture.

Interpreter requirements:

- Dispatch is benchmarked before broad feature work. Choose accumulator vs
  register, match dispatch, or threaded-dispatch variants based on measurement.
- Hot opcodes have explicit fast paths with no hidden allocation.
- Frame layout is compact and cache-conscious: locals, temporaries, arguments,
  return address, environment pointer, and feedback vector access are planned
  together.
- Common calls do not allocate. Small argument lists use inline storage or
  caller-owned frame slots.
- Property access goes through a real inline-cache path early, not a late
  optimization pass.
- Runtime checkpoints are centralized and cheap enough to call at back-edges and
  native-loop polling sites.
- Every slow path is observable in traces/profiles so performance work can be
  guided by data.
- Interpreter performance is the foundation target. Do not rely on "the JIT will
  fix it later" as a design argument.
- The interpreter is allowed to fall back to generic semantics. It is not
  allowed to paper over missing semantics with wrapper allocation, UTF-8
  conversion, unordered maps, or recursive algorithms that will later need a
  rewrite.

Design review gate:

- Before implementing a major VM subsystem, write the invariants first:
  ownership, allocation behavior, GC rooting behavior, interrupt behavior,
  error behavior, and benchmark target.
- A subsystem is not accepted until it has unit tests, at least one CLI/runtime
  fixture through `otter test`, and a benchmark if it sits on a hot path.

## Vertical Slice Policy

Move in small, boring, high-quality slices. Do not start by implementing "the JS
runtime". Start by making one primitive or one operation family excellent, wire
it through parser -> compiler -> bytecode -> interpreter -> runtime API -> CLI ->
`otter test` -> benchmark, then move to the next slice.

Each slice must include:

- exact JS surface covered
- exact TypeScript syntax accepted or intentionally rejected
- representation and invariants
- bytecode instructions and dispatch behavior
- allocation/rooting behavior
- error semantics
- `otter test` fixtures
- targeted Test262 subset when one exists
- Criterion benchmark when the slice is hot
- docs/API updates only if the surface is user-visible

Early slice order:

1. **TypeScript frontend skeleton:** `.ts` file loading, parser mode, type-only
   syntax stripping, source span preservation, and diagnostics.
2. **String core:** literals, allocation, length, concatenation, code-unit
   indexing, equality, display/debug output.
3. **String methods:** `charCodeAt`, `charAt`, `slice`, `substring`, `indexOf`,
   `startsWith`, `endsWith`; still no broad object work.
4. **Number core:** numeric literals, int32/double representation, arithmetic,
   comparison, `NaN`, `-0`, infinities, and `ToNumber` for already-supported
   primitive conversions.
5. **Boolean/nullish core:** `true`, `false`, `null`, `undefined`, equality,
   truthiness, branching.
6. **Local control flow:** variables, blocks, loops, branches, back-edge
   checkpoints, and stack-depth limits.
7. **Calls:** function declarations, fixed-arity calls, return values, `this`
   placeholder semantics, and no-allocation small calls.
8. **Object/property core:** object literals, ordered own properties, shape
   transitions, named load/store ICs.
9. **Primitive receiver lookup:** `"x".length`, string prototype methods, number
   and boolean prototype lookup without wrapper allocation.
10. **Arrays:** dense arrays first, sparse fallback later.
11. **Builtins by family:** only after the underlying primitive/object model is
    solid.

No slice is allowed to pull in a broad compatibility shim to fake progress. If a
feature needs a missing lower-level semantic, implement that semantic first or
cut the slice smaller.

## Repository Cleanup Policy

The repository needs aggressive cleanup before the new foundation can move
quickly. Default stance: if a file does not help build, test, benchmark, document,
or ship the new engine path, it is trash and should be deleted in an explicit
cleanup commit.

This especially applies to:

- stale benchmark scripts and benchmark result dumps
- one-off debug scripts
- obsolete docs and duplicate roadmap files
- generated traces, heap snapshots, profiles, timeout dumps, and test result
  directories
- old fixtures that are not run by `cargo test`, `otter test`, or the Test262
  runner
- compatibility experiments that are not part of the active runtime path

Every suspicious file, module, and crate gets one of three outcomes:

- **Keep active:** required by the new engine, public API, CLI, CI, or current
  conformance work.
- **Keep parked:** temporarily retained old code with a named reason, no active
  dependency edges, and an exit condition.
- **Delete:** everything else.

Rules:

- Active code stays in the workspace build graph and passes the full quality
  gate.
- Parked code is not imported by active crates and does not receive new features.
- Delete commits are allowed to be blunt, but they must be separate from runtime
  behavior changes.
- Generated artifacts and local test output are deleted unless they are named
  golden fixtures used by an active test command.
- Old docs are deleted or replaced by a short superseded-by link. Do not keep a
  museum of conflicting plans.
- Benchmark code is kept only if it is wired into the current benchmark workflow
  and has a named performance question. Random scripts go away.
- Test fixtures are kept only if a checked-in runner executes them.

First cleanup targets:

- Delete stale root docs and duplicate plans after adding one current foundation
  plan and one small repository map.
- Delete `test262_results/`, benchmark outputs, traces, heap snapshots, timeout
  dumps, and local profiler artifacts unless explicitly converted into fixtures.
- Delete benchmark and script directories that are not wired into CI or an
  active `just` command.
- Delete old fixture trees that are not executed by `otter test`, `cargo test`,
  or the dedicated Test262 runner.
- Audit parked crates and remove any dependency edge from active runtime crates
  into parked compatibility code.
- Add CI checks that fail on committed trace/profile/result artifacts outside
  approved fixture directories.

## Milestones

### M0: Repository Guardrails

Goal: make it impossible to accidentally grow another broken stack.

- Add a short architecture decision record for the new engine path.
- Define which active crates own the foundation work.
- Ensure new crates or modules cannot bypass `#![forbid(unsafe_code)]` except in
  `otter-gc` and `otter-jit`.
- Add CI checks for formatting, clippy, tests, and test-count floor.
- Add benchmark CI plumbing before making performance claims.
- Establish the first conformance baseline and record it in the repo. If
  `ES_CONFORMANCE.md` is absent, recreate it before feature work starts.
- Freeze the first public API sketch in an ADR: what is stable, what is
  experimental, and what stays private.
- Freeze the first CLI command contract in docs before implementation churn
  starts.
- Decide the staging directory for new foundation crates and document the
  promotion/rename rule.
- Add a parser decision note: OXC is mandatory for JS/TS parsing and syntax
  spans.

Acceptance:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`
- A documented baseline for unit tests, conformance subset, and core
  microbenchmarks.
- A documented public API and CLI contract with examples.

### M1: Public API and CLI Skeleton

Goal: make the new engine usable from Rust and from the command line while the
VM is still small.

Rust API surface:

- `RuntimeBuilder` for capabilities, heap cap, timeout, module loader, console,
  and profiling/trace sinks.
- `Runtime::run_script(source, specifier)`.
- `Runtime::run_module(specifier)` once modules are available.
- `SourceKind::JavaScript` and `SourceKind::TypeScript` or equivalent source
  detection, with `.ts` treated as first-class input.
- `Runtime::eval(source)` for tooling and tests, with clear non-production
  semantics if eval is incomplete.
- `ExecutionResult` that can expose completion value, stdout/stderr capture when
  configured, diagnostics, exit status, timing, and optional profiling outputs.
- `Diagnostic` with source span, code frame data, cause chain, and machine
  readable error kind.
- `CapabilitySet` with deny-by-default fs/net/env/subprocess/ffi settings.

CLI surface:

- `otter file.ts args...` as shorthand for running a script.
- `otter run file.ts args...`.
- `.js`, `.mjs`, `.ts`, and future `.mts` inputs route through explicit source
  kind handling, not filename hacks buried in the compiler.
- `otter eval 'expr'` and `otter -p 'expr'`.
- `otter check file.ts` for parse/compile/type-check plumbing as it becomes
  available.
- `otter test [path]` as the first-party engine test harness for runtime
  regression tests, fixture suites, smoke suites, and curated Test262 subsets.
  It is not a Jest clone yet.
- `otter info` for build/runtime feature flags.
- Common controls: `--timeout`, `--max-heap-bytes` or equivalent,
  `--allow-read`, `--allow-write`, `--allow-net`, `--allow-env`, `--trace`,
  `--cpu-prof`, and `--json`.

Requirements:

- CLI paths use the public Rust API, not private VM entry points.
- User-facing errors are structured diagnostics, not debug strings.
- JSON output mode is stable enough for CI and benchmark automation.
- Every CLI command has integration tests.
- `otter test` can run the engine's own tests without cargo-specific glue, so
  the same command is usable by contributors, CI, and release smoke checks.

### M2: TypeScript Frontend and Minimal VM Harness

Goal: execute the first vertical slices with a production frame model, not a
wide JS subset. TypeScript input is accepted from this point forward.

Scope:

- Oxc parser integration for JavaScript and TypeScript modes.
- Type-only TypeScript syntax stripping/lowering for the supported subset:
  annotations, interfaces/types as erased declarations, `as` expressions, and
  basic generic syntax only when it can be erased safely.
- Clear diagnostics for unsupported TypeScript syntax instead of miscompilation.
- Bytecode container, instruction encoding, constants table, source spans, and
  disassembly.
- Interpreter loop with completion values, structured errors, timeout, heap cap,
  and tracing hooks.
- Bytecode dump and disassembly in human-readable and machine-readable forms.
- Instruction trace hooks with source span and function/module identity.
- Minimal literal loading for the current slice only.
- `otter run`, `otter eval`, `otter -p`, and `otter test --suite engine` execute
  through this path.
- Source spans on bytecode for diagnostics and profiling.

Performance requirements:

- Dispatch loop has a measured baseline with Criterion.
- Call frames do not allocate for small arity calls.
- Back-edge interrupt checks are cheap and centralized.

Correctness requirements:

- No unchecked recursion in compiler or VM debug paths.
- TypeScript source spans survive into diagnostics and stack traces.
- Unsupported TS syntax fails at check/compile time with structured diagnostics.
- A failing fixture can be debugged from CLI output using source diagnostics,
  bytecode dump, and instruction trace without adding ad-hoc prints.
- Stack-depth limit returns a catchable JS error.
- OOM before allocation mutation is covered by tests.

### M3: String Core Slice

Goal: make JS strings correct and measurable before adding broad language
surface.

Scope:

- Canonical value tags needed for `String`, `undefined`, and completion values.
- One string allocation path with WTF-16 backing.
- String literals, equality, length, display/debug output, and `+` string
  concatenation.
- Rope `Cons`, `Sliced`, and `Thin` representations with lazy flattening.
- Heap accounting for string backing stores.
- CLI/API fixtures for printing and comparing strings.
- Benchmarks for literal load, equality, concat loop, flatten, and length.

Forbidden:

- UTF-8 round-trips for JS string semantics.
- Eager general-purpose string concat.
- Multiple string heaps in active execution.

Acceptance:

- Lone surrogates survive allocation, comparison, printing diagnostics, and
  indexed access once indexing lands.
- `s += piece` loop is not O(n²).
- OOM before string allocation mutation is covered by tests.

### M4: String Methods Slice

Goal: finish the first practical string operation family without pulling in the
whole object model.

Scope:

- `length`, `charCodeAt`, `charAt`, `slice`, `substring`, `indexOf`,
  `startsWith`, `endsWith`, and comparison on code units.
- Lone surrogate preservation in all string APIs.
- Primitive receiver method dispatch for supported string methods without
  wrapper allocation.
- Lazy flattening only where required by the operation, with depth and size
  safeguards.
- Criterion benchmarks for concat loops, indexing, slicing, and search.

Acceptance:

- No UTF-8 round-trip in indexed string operations.
- `s += piece` loop is not O(n²).
- Test262 string subset baseline improves or stays stable with documented gaps.

### M5: Number Core Slice

Goal: implement numeric semantics narrowly and correctly before object/array
work.

Scope:

- Number literals, int32 immediate path, double path, `NaN`, `-0`, infinities.
- Arithmetic: `+`, `-`, `*`, `/`, `%`, unary `-`, increment/decrement if parser
  support exists.
- Comparisons for supported primitives.
- `ToNumber` only for primitives already implemented by earlier slices.
- Benchmarks for int32 loops, double loops, mixed numeric operations, and
  comparison branches.

Requirements:

- `-0`, `NaN`, and infinity behavior has dedicated fixtures.
- Integer fast paths do not allocate.
- Falling from int32 to double is explicit and tested.

### M6: Boolean, Nullish, and Control Flow Slice

Goal: make branching, truthiness, and loops solid before calls and objects.

Scope:

- `true`, `false`, `null`, `undefined`.
- Strict equality and loose equality only for supported primitive pairs.
- Truthiness and conditional branches.
- Blocks, local bindings, `if`, `while`, `for`, `break`, `continue`.
- Back-edge checkpoints and interrupt tests.

Requirements:

- Branch and loop fixtures run through `otter test`.
- Infinite loops are interruptible.
- Unsupported coercions fail clearly instead of silently faking semantics.

### M7: Calls and Frames Slice

Goal: add functions without allowing per-call allocation or sloppy frame design.

Scope:

- Function declarations/expressions for the supported subset.
- Fixed-arity calls, extra/missing argument handling, return values.
- Compact frame layout with no allocation for common small calls.
- Stack-depth limit with catchable JS error.
- Source spans in stack traces.

Requirements:

- Call overhead benchmark exists before broad builtin work.
- Small calls do not allocate temporary `Vec`s.
- `arguments`, closures, and rest parameters are separate later slices unless
  explicitly included with tests and benchmarks.

### M8: Objects, Shapes, and Property Access

Goal: property access must be correct and fast before broad builtins land.

Scope:

- Hidden classes/shapes with stable property offsets.
- Ordered own-property storage for observable enumeration.
- Prototype chain lookup with receiver separated from lookup target.
- Primitive receiver lookup without wrapper allocation.
- Inline cache feedback slots for named loads and stores.
- Shape transitions for object literals and property additions.

Performance requirements:

- Monomorphic named load fast path.
- Polymorphic named load cache with a small fixed shape budget.
- Microbenchmarks for object property read/write, prototype read, and primitive
  string `.length`.

Correctness requirements:

- Property order tests for `Object.keys`, `for...in`, and JSON object keys.
- No wrapper object allocation for `"x".length` or `"x".charCodeAt(0)`.

### M9: Arrays and Builtin Skeleton

Goal: support enough real code to start meaningful conformance work.

Scope:

- Dense arrays with length semantics.
- Sparse fallback representation.
- `Array.prototype` essentials: `push`, `pop`, `map`, `forEach`, `reduce`,
  `slice`, `concat`, iterator.
- Function objects, prototypes, constructors, `this` binding, and `new`.
- Lazy `arguments` object creation only when the compiler proves it is needed.
- Microtask/job queue skeleton for promises, without broad async APIs yet.

Requirements:

- Native loops poll interrupts.
- Array iteration preserves holes and spec-visible order.
- No per-call `Vec` allocation for common call shapes.

### M10: Conformance Ratchet

Goal: start closing Test262 by feature area without destabilizing the core.

Scope:

- Establish feature-area dashboards.
- Track JavaScript and TypeScript frontend regressions separately: Test262 for
  ECMAScript behavior, `otter test --suite engine` fixtures for TypeScript
  lowering/source-span behavior.
- Pick one feature family at a time.
- For each family, record before/after pass rate and top failure categories.
- Add unit tests for every bug fixed below the Test262 level.

Policy:

- No conformance regression without an explicit tracked exception.
- No "compatibility shim" that bypasses the VM semantics.
- Public API, TypeScript declarations, docs, and examples update together when a
  surface becomes user-visible.

### M11: Developer Loop and Runtime Test Runner

Goal: make daily engine development fast enough that correctness and performance
regressions are caught immediately.

Scope:

- `otter test` can run `.js`/`.ts` runtime tests with deterministic ordering,
  per-test timeout, heap cap, and filtered execution.
- `otter test --suite engine` runs first-party VM/runtime fixtures.
- `otter test --suite smoke` runs short release smoke tests.
- `otter test --suite test262 --filter <pattern>` runs curated conformance
  subsets through the same runtime path, with the dedicated test262 runner kept
  for full corpus reporting.
- `otter test --bless` updates golden outputs only for approved fixture
  directories.
- Test output has human and `--json` modes.
- Snapshot/golden tests are supported for diagnostics and stack traces.
- `otter check` can parse TypeScript, strip/lower supported type syntax, compile,
  and report structured diagnostics without executing.
- `otter bench` may remain later, but benchmark binaries must be runnable from
  CI before performance-sensitive changes land.

Requirements:

- The test runner uses isolates/runtimes the same way embedders do.
- A hung test cannot hang the whole suite.
- Test count, pass count, timeout count, and ignored count are machine readable.
- Engine tests can assert stdout, stderr, completion value, thrown diagnostic,
  exit code, timeout, heap-limit failure, and trace/profiling artifact shape.
- `otter test` is dogfooded in CI for VM/runtime fixtures before broad user
  test-runner compatibility work begins.

### M12: Repository Cleanup Pass

Goal: reduce noise so active engine work is obvious and accidental coupling is
hard.

Scope:

- Mark every crate as active, parked, reference-only, or delete candidate.
- Move stale design notes into `docs/archive/` with a short superseded-by link.
- Remove checked-in generated output unless it is a named golden fixture.
- Delete dead modules only after `rg` proves no active references and tests cover
  the replacement path.
- Add workspace lints for accidental active-to-parked dependencies.
- Add a small script or CI check that reports untracked generated result
  directories and known dump file extensions.

Acceptance:

- `docs/repository-map.md` exists and matches `Cargo.toml` workspace members.
- Active crates do not depend on parked compatibility crates.
- CI rejects accidental committed profiler traces, heap snapshots, local
  test262 result dumps, and benchmark result dumps outside approved locations.
- Cleanup commits do not change runtime behavior except by deleting unreachable
  code.

## Performance Baseline From Day One

The interpreter is allowed to be incomplete. It is not allowed to be casually
slow. The first benchmark suite must include:

- integer loop arithmetic
- function call overhead
- closure call overhead once closures exist
- named property load/store
- prototype property load
- primitive string `.length`
- string concat loop
- string `indexOf`/`slice`
- dense array `push`/iteration
- JSON stringify/parse once JSON lands
- CLI startup for `otter -e ""` and `otter run hello.js`
- CLI startup for `otter run hello.ts`
- public API startup for `RuntimeBuilder::build()` + first script execution
- TypeScript parse/strip/lower throughput for supported syntax

Every benchmark must specify:

- input size
- allocation count or bytes where measurable
- comparison target (`node`, `bun`, `deno`, or previous Otter baseline)
- expected classification: correctness-only, smoke, or regression gate

Do not claim a performance improvement without a benchmark committed in the same
change.

## Testing Floor

The project needs a ratchet, not vibes:

- Unit test count must not decrease unless the commit explains why.
- Test262 pass count for the touched feature must not decrease.
- Benchmarks must compile in CI.
- Panic tests must prove JS errors are catchable where the runtime should
  recover.
- OOM tests must verify no heap mutation after rejected allocation.
- Interrupt tests must cover bytecode loops and native loops separately.

## Immediate Task List

1. Create or restore `ES_CONFORMANCE.md` with the current baseline and known
   missing areas.
2. Add an ADR for the new foundation path and active-crate ownership.
3. Add an ADR for public Rust API and CLI command shape.
4. Decide and create the temporary staging directory for new crates, with a
   written promotion/rename rule.
5. Add an ADR that OXC is the parser/frontend dependency for JS/TS.
6. Define `otter test` as the engine harness command and specify its fixture
   format, suites, timeout behavior, and JSON schema.
7. Define bytecode dump, disassembly, and trace output formats before the first
   instruction set grows.
8. Add the first repository cleanup map: active, parked, reference-only, delete
   candidate.
9. Add CI/test-count floor enforcement.
10. Add the first Criterion benchmark crate or `benches/` targets for VM dispatch,
   calls, strings, and property access.
11. Define the canonical value and string representation before writing any
   broad builtins.
12. Implement the smallest public API + CLI path over the VM harness.
13. Land the TypeScript frontend skeleton end to end.
14. Land the String core slice end to end.
15. Land String methods end to end.
16. Land Number core end to end.
17. Land Boolean/nullish/control-flow end to end.
18. Land calls/frames end to end.
19. Only then expand into objects, arrays, descriptors, and broad builtins.

## Definition of Done for Foundation

The foundation phase is done when Otter has:

- one active value representation
- one active string representation family
- one active object/shape model
- one fallible allocation path
- a thin stable public runtime API
- CLI run/eval/check/test commands backed by that API
- `otter test` running the engine fixture suites with deterministic JSON output
- TypeScript input supported out of the box for the foundation subset
- OXC is the only JS/TS parser in the new foundation path
- bytecode dump, disassembly, and source-span traces work from the CLI
- a measured interpreter dispatch baseline
- primitive receiver property lookup without wrapper allocation
- ropes for general string concatenation
- deterministic observable property order
- hard heap caps
- interruptible bytecode and native loops
- enforced unsafe boundaries
- conformance and benchmark ratchets in CI
- no JIT dependency for correctness or baseline performance claims

At that point, expanding into Web APIs, Node compatibility, package tooling, FFI,
and JIT tiers becomes engineering work on top of a stable VM instead of another
round of debt migration.
