# AGENTS.md
For any file search or grep in the current git-indexed directory, use fff tools.

Guidance for coding agents (Claude Code / Codex CLI / etc.) when working in this repository.

## Project Overview

Otter is an embeddable TypeScript/JavaScript engine for Rust applications built on a custom bytecode VM. It provides a safe runtime for executing TypeScript/JavaScript code with native Rust integration, plus a standalone CLI.

**Workspace naming:** the active CLI crate is `otter-cli` under `crates/`; legacy `crates-legacy/*` paths are reference-only unless a task explicitly says otherwise.

> **Note:** The VM is under active development. Some features (full Web APIs) are being added incrementally.

## Runtime Layout

The active runtime stack is:

- `crates/otter-gc`
- `crates/otter-vm`
- `crates/otter-runtime`
- product crates under `crates/*`

Do not introduce parallel engine/runtime stacks through copied modules, renamed
crates, or path dependencies.

Repository rules:

- New runtime, VM, Web API, extension, Node.js API, FFI, KV, and SQL work belongs on the active runtime stack.
- Keep dependency direction simple: `otter-gc` -> `otter-vm` -> `otter-runtime` -> product crates.
- Prefer vertical slices and small ports over large framework rewrites.
- Keep parked compatibility shims out of the active workspace build graph.

## Pre-Release Architecture Policy

Otter is currently pre-user and pre-stability. Improve the architecture
directly, even when that breaks an internal API, ABI, diagnostic schema,
artifact format, or test fixture.

- Do not version internal contracts: do not add or bump format/schema
  versions, migrations, compatibility readers, dual writers, deprecated
  aliases, or legacy modes for in-repository contracts. Change the one current
  format in place and update every producer, consumer, fixture, test, and
  document in the same patch.
- Do not add bridges, adapter layers, generic-call detours, or replay paths to
  connect old and new engine designs. Refactor both sides onto the intended
  final boundary.
- Prefer a clean breaking change when it removes debt or improves correctness,
  introspection, code generation, or runtime performance.
- Versioning or compatibility work requires an explicitly declared external
  consumer contract and explicit task approval; never infer that requirement.

## Module Size And Boundary Hygiene

Do not grow kilometer-long `lib.rs` files. New non-trivial runtime,
compiler, resolver, package-manager, or diagnostics behavior belongs in a
focused module with an LLM-friendly top-level `//!` docstring that explains:
short purpose, `# Contents`, `# Invariants`, and `# See also` when relevant.
Keep `lib.rs` as the crate map, public re-export surface, and small glue only.
When touching an already oversized `lib.rs`, prefer extracting the new work to a
named module instead of adding more large type/function blocks.

Do not expose `Rc`, `RefCell`, raw VM handles, or other single-threaded interior
mutability types across runtime/compiler/package-manager public boundaries.
New boundary DTOs should be owned data (`String`, `Vec`, maps with deterministic
ordering where needed) and remain `Send`/`Sync` friendly. Existing internal
compiler builder state that uses `Rc<RefCell<_>>` is legacy implementation
detail; do not expand it or copy the pattern into runtime/session APIs.

## Agent Checklist (per task)

1. **Confirm intent + constraints**: Web API compatibility? sandbox/permissions? performance target? platform?
2. **Check ES conformance status**: Before working on feature implementation or bug fixes, consult `ES_CONFORMANCE.md` to understand the current pass rate for the affected area.
3. **Search before adding**: use `fff` in this git-indexed repository; fall
   back to `rg` only when the `fff` service is unavailable.
4. **Keep patches surgical**: avoid refactors unless requested; keep public APIs stable.
5. **Respect safety boundaries**: follow the `unsafe` rules and GC invariants below.
6. **Update the "triangle" when needed**: runtime behavior ↔ TypeScript `.d.ts` ↔ docs/examples/tests.
7. **Parse JS/TS with ASTs**: use `oxc`/SWC; never regex-parse JS/TS.
8. **Protect the runtime boundary**: do not add dependencies from active crates into parked compatibility shims.
9. **Prefer build-graph cleanup**: when a slice lands, remove temporary shims and parked code from the active path immediately.
10. **Use porting markers for uncertain migrations**: for substantial ports from parked shims or reference implementations, follow `docs/site/src/content/docs/contributing/porting.md` (`TODO(port)`, `PERF(port)`, `PORT NOTE`, optional `PORT STATUS`).

## Repository Map (where to change what)

### Core Runtime Crates
- `crates/otter-gc`: garbage collector.
- `crates/otter-vm`: VM, interpreter, value model, intrinsics, source compiler.
- `crates/otter-runtime`: public runtime and embedding surface.

### Supporting crates
- `crates/otter-bytecode`: bytecode representation and disassembly.
- `crates/otter-syntax` / `crates/otter-compiler`: frontend and lowering.
- `crates/otter-test262`: active ECMAScript conformance runner.
- `crates/otter-cli`: CLI (`otter`).
- Future macro / module / Web API crates must be added under `crates/*`, not under legacy `crates-legacy/*`.

### Parked Compatibility Shims
- `crates-legacy/otter-nodejs`
- `crates-legacy/otter-node-compat`

## File Naming Conventions

### Rust Module Documentation

Every new or materially changed Rust module must keep its top-level
`//!` docstring accurate in the same style as the rest of the active
engine: short purpose, `# Contents`, `# Invariants`, and `# See also`
sections when the module owns non-trivial runtime behavior. When a task
removes a known limitation, update or delete stale "foundation gap",
"task N", and "TODO until GC" wording in the same patch as the code.
Docstrings should describe the current production behavior, not the
history of how the port got there.

### Builtin Modules

For builtin modules use the following naming scheme:

| File             | Purpose                                |
|------------------|----------------------------------------|
| `module_ext.rs`  | Rust implementation of native functions |
| `module.js`      | JavaScript shim / polyfills            |

Example for `fs` module:
- `fs_ext.rs` — native functions: `readFile`, `writeFile`, `stat`, etc.
- `fs.js` — JS wrappers, exports, additional logic

This separation:
- Clearly distinguishes Rust and JS code
- Makes it easy to find the right implementation
- Maintains consistency across modules

### Intrinsic and Bootstrap Pattern

New ECMAScript builtins, global namespaces, Web API globals, and extension-visible host objects must follow the descriptor/spec/builder/bootstrap flow documented in `docs/site/src/content/docs/extensions/js-surface-builders.md`.

- Add new bootstrap work in `crates/otter-vm` / `crates/otter-runtime`.
- Keep global installation centralized; do not scatter ad-hoc global mutation across unrelated modules.
- Prefer static specs plus mutator-bound builders over one-off registration functions when exposing JS-visible constructors, prototypes, and namespaces.
- High-level APIs must compile down to the same runtime shape as handwritten static specs: no per-call allocation, runtime metadata parsing, or hot-path dynamic registry.
- Contributor-facing workflow docs belong in `docs/site/src/content/docs/`; task files are implementation history.
- If a feature exists only in parked code, port or redesign it; do not grow the parked surface.

## Development Philosophy

- **Production-ready code**: No premature micro-optimizations. Write clean, idiomatic Rust first.
- **Performance target**: High-performance execution with competitive benchmarks.
- **API compatibility**: Prioritize compatibility with web standards.
- **AST-first parsing**: Use ASTs via `oxc`/SWC for JS/TS analysis or transforms; do not use regex to parse JS/TS code.
- **Idiomatic Rust**: Follow Rust best practices, use proper error handling, leverage the type system.
- **Secure defaults**: deny-by-default permissions; new capabilities must be explicit and testable.

## Macro and Async Agreements

### Macro usage

Macros are planned as zero-cost contributor ergonomics over the static spec / builder backend. Do not add macro-first APIs that bypass the builder/bootstrap layer.

Initial macro scope:

- `#[js_class]` for constructor-backed JS classes
- `#[js_namespace]` for namespace-style JS objects
- `raft!` or equivalent grouped static-spec declaration

Deferred until their backend APIs are stable:

- `#[dive]` / async native binding sugar
- `burrow!` for host-owned object surfaces
- `lodge!` for hosted module declarations
- GC trace derive macros

Rules:

- Macros must generate static specs plus normal Rust functions; they are syntax sugar over JS surface builders, not a parallel runtime registry.
- Generated builtins should use the static native function-pointer path by default.
- Keep exported JS names and arity explicit in the macro declaration. Do not hide API shape in unrelated helper code.
- If a macro-based API surface changes, update tests, `.d.ts` declarations, and Astro docs in the same patch when applicable.

Keep code manual when:

- capability enforcement is the main behavior
- bootstrap/install order is delicate
- the macro would hide important control flow

### Async model

- Timers are runtime primitives, not Node-specific APIs:
  - `setTimeout`, `setInterval`, `setImmediate`, `queueMicrotask` belong in the core runtime.
  - `node:timers` / `timers`, if exposed again, must re-export runtime globals rather than grow a separate backend.
- Promise settlement for async native APIs must go through the target VM/runtime job queue.
- Worker tasks may execute plain Rust async work, but VM/JS interaction must hop back onto the runtime scheduling boundary.
- For stream/iterator-like async APIs, use explicit pending queues and deterministic delivery semantics.
- When reviving parked async APIs, move semantics first and wire the final runtime boundary second; do not preserve old abstractions just for compatibility.

## Common Pitfalls to Avoid

### 1. Wrong Collection Type
**Problem**: Using `HashMap`/`FxHashMap` when output order matters (JSON, iterators).
**Solution**: Use `BTreeMap` or `IndexMap` for deterministic iteration order.

```rust
// JSON object keys must preserve insertion order per spec
use indexmap::IndexMap;
struct JsObject {
    properties: IndexMap<String, Value>,  // NOT HashMap
}
```

### 2. Unbounded Recursion
**Problem**: Stack overflow on deeply nested structures (JSON, AST, objects).
**Solution**: Add depth limits or use iterative algorithms with explicit stack.

```rust
const MAX_NESTING_DEPTH: usize = 512;

fn stringify(value: &Value, depth: usize) -> Result<String, Error> {
    if depth > MAX_NESTING_DEPTH {
        return Err(Error::TooDeep);
    }
    // ... recurse with depth + 1
}
```

### 3. Forgetting GC Roots
**Problem**: `Value`/`JsObject`/`JsString` are raw `Copy` cage offsets; the young
generation is a moving collector, so a value held in a Rust local goes stale
(and is later silently "laundered" into a wrong object) the moment a later
allocation triggers a collection.
**Solution**: Build values inside a handle scope — `ctx.scope(|ctx, s| …)`
with the `scoped_*` methods. Handles live in a collector-traced arena and can
never go stale; the compiler stops them escaping the scope. **This is the
standard API for all native value building** — see
[the handle-scopes doc page](docs/site/src/content/docs/extensions/handle-scopes.md).
Do not add new code with the
deprecated manual `value_roots` threading or raw-`Value` juggling; verify any
multi-allocation native under `OTTER_GC_STRESS=1..16` (identical output every
stride).

### 4. Non-deterministic Test Failures
**Problem**: Tests pass/fail randomly due to hash map iteration order.
**Solution**: Sort keys before comparison, or use ordered collections throughout.

## Build Commands

```bash
# Build
cargo build                          # Debug build
cargo build --release -p otter-cli     # Release CLI binary
cargo build --profile checked -p otter-cli # Optimized binary with invariant checks

# Test
cargo test --all --all-features      # Run all tests

# Lint
cargo fmt --all                      # Format code
cargo clippy --all-targets --all-features -- -D warnings

# Run scripts
cargo run -p otter-cli -- run <file>   # Run a script
cargo run -p otter-cli -- check <file> # Type check with tsgo

# Quick local loop
just quick calls
```

Justfile shortcuts available: `just fmt`, `just lint`, `just test`, `just build`, `just release`

Fast iteration tips:
- For JIT/GC hypotheses, start with the affected test and
  `just quick [calls|gc|math|native|properties|all]`. The default `calls` family
  runs the two constructor moving-GC regressions; `gc` covers collector and
  remembered-array safety, `math` guarded methods, `native` resolved calls and
  moving argument roots, and `properties` named slots and shared-cache loads.
  These incremental checks default to stress stride 1 and preserve an explicit
  stress value and Cargo target. Run the full gate and required targeted
  Test262 checks once the coherent change is ready, rather than after each edit.
- For closing moving-GC checks, use `target/checked/otter` with
  `python3 scripts/dev/gc-stress.py target/checked/otter benchmarks/results/<fresh-dir> --all --strides 1-16`.
  `--repeat 100` repeats every corpus/tier/stride case for flake diagnosis;
  `runs.jsonl` preserves each completed observation during an interrupted run.
  Console output and printed completion are compared with the unstressed
  interpreter, including programs whose only observable result is their final value.
  `results.json` must report `status: passed` before the run counts as a gate.
- Run VM tests: `cargo test -p otter-vm`
- Run runtime tests: `cargo test -p otter-runtime`
- Run a single active support crate after porting work there: `cargo test -p otter-modules`, `cargo test -p otter-web`, etc.

## Architecture

### Crate Hierarchy (bottom to top)

```
otter-cli (CLI -> `otter`)
    ↓
    host/runtime integration layer
    ↓
otter-runtime
    ↓
otter-vm
    ↓
otter-gc
```

Supporting crates should live under `crates/*`. Legacy crates under
`crates-legacy/*` are reference-only and must not be added to the active build
graph.

### Key Architectural Constraints

1. **GC Safety**: Values must be properly rooted when stored across GC boundaries. Use the active GC/reference types and rooting patterns.

2. **Value Representation**: The value model lives in `crates/otter-vm/src/lib.rs`.

3. **Object Model**: The object model lives in `crates/otter-vm/src/object.rs`.

4. **Async ops require Tokio**: async ops are scheduled onto a Tokio runtime handle (thread-local).

5. **TypeScript Pipeline**: Compilation via oxc parser. Type checking via tsgo (to be re-enabled).
6. **AST-only parsing**: Use ASTs via `oxc` for JS/TS analysis or transforms; no regex parsing.

### Builtin Functions

Native functions are registered via runtime/engine extensions. Example:
```rust
use otter_vm::descriptors::VmNativeCallError;
use otter_vm::value::RegisterValue;

pub fn console_log(args: &[RegisterValue]) -> Result<RegisterValue, VmNativeCallError> {
    for arg in args {
        print!("{arg:?}");
    }
    println!();
    Ok(RegisterValue::undefined())
}
```

Native/builtin contributors on the active stack should allocate and
mutate through the explicit context APIs (`NativeCtx::alloc[_old]`,
`NativeCtx::record_write`, `NativeCtx::reserve_external`, and branded
`GcSession` entry). Use `EscapableHandleScope` when returning one
`Local` out of a nested handle scope. Do not expose or call raw heap
mutation, raw slot visitors, `otter_gc::raw::*`, or manual write
barriers from contributor-facing code.

The compiled-code ABI is private engine plumbing, not an extension or FFI
contract. `NativeFrame`, safepoint/root records, runtime-stub ids, and compiled
result pairs may cross `otter-jit` ↔ `otter-vm` only. Native extensions and any
future Node-facing modules must enter through the high-level `NativeCtx` /
handle-scope API; do not reuse JIT frame or status layouts as a second GC entry.
Physically identical engine carriers must have one owner and one type. Express
different legal status subsets through descriptor-domain validation, not
parallel structs, aliases, constructors, decoders, or duplicate test matrices.
On x86_64, generated JavaScript calls retain the private actual-only stack ABI;
Rust helpers and external tier/OSR entries use the platform C boundary. Use
`x86_64::call_abi` at C call sites, including Microsoft shadow space and hidden
pair results; do not route private `JsCall` entry-cell or tail transfers through
it. Shared frame emitters initialize canonical roots before publication and
restore prior interpreter root words on every tier exit.

## Platform Support

Pure Rust implementation - no external JavaScript engine dependencies.

- **macOS**: x86_64, ARM64
- **Linux**: x86_64, ARM64
- **Windows**: x86_64

## Debugging

- Long-running scripts/servers: use `--timeout 0` (disables the timeout).
- Normal CLI execution always uses the production tier policy. `--jitless`
  runs the template baseline tier without optimizing compilation;
  `--interpreter` runs the bytecode interpreter alone and is the semantic
  oracle for differential comparisons (`otter-difftest` uses it). There is no
  finer-grained user-facing tier selector.
- When editing embedded JS shims: they are compiled in via `include_str!` and passed through `CString::new(...)` (no `\0` bytes).
- Bytecode disassembly (compile and exit):
  - Text: `cargo run -p otter-cli -- --dump-bytecode <file>`
  - JSON: `cargo run -p otter-cli -- --dump-bytecode=json <file>`
- VM step trace for a script:
  - Stderr: `cargo run -p otter-cli -- --trace=- run <file>`
  - File: `cargo run -p otter-cli -- --trace=otter-trace.txt run <file>`
  - The shipped step trace uses one current text format and records
    interpreter-dispatched opcodes only. A `.json` extension does not change
    its format, and native JIT bodies do not currently emit step events.
- Structured JIT events:
  - Default artifact path:
    `cargo run -p otter-cli -- --jit-events run <file>`
  - Explicit path:
    `cargo run -p otter-cli -- --jit-events=/tmp/otter-jit-events.json run <file>`
  - `--jit-events=-` writes the JSON report to stderr. Prefer a file when the
    program itself uses stderr or when `--json` is active.
  - The current report contains typed compile, post-hook installation refusal,
    inlining, direct-call plan/final-lowering, bail, generated-call-deopt, and
    inline-deopt events. Bounded method chains expose `targetIndex` /
    `targetCount`; Graph `inlineLowered` reports actual accepted splices with
    parent/callee, depth and their charged encoded-bytecode-byte budget cost.
    `inlineCandidate` reports VM bake refusals; Graph does not currently emit
    backend splice rejections. Template direct-call lowering retains its typed
    final outcomes. A reached site without proven function-cell linkage reports
    `backendUnsupported`, including the current x86 method resolver/generic path.
    `installDeclined` identifies the exact code object and
    reports `invalidCode` or the full resource requirement and available
    physical headroom. `compilePrepared.sourceWork` reports entered source-opcode
    attempts visible before the hook; `compilePrepared` reports `directConstructs`, `directMethodSites` and
    `directMethodTargets` separately from body-inline candidate counts, while
  `bindingSites` counts schema-typed global, captured, dynamic, and shadowed
  binding accesses; `bindingHitProofs` counts stable global-declarative cells
  and guarded global-object slots available for generated hits;
  `stringConstantCells` counts eagerly prepared stable traced literal cells. Capture is
    default-off and bounded to 16,384 events per top-level run; `truncated`
    and `droppedEvents` report overflow without constructing further payloads.
  - Abrupt VM completion (for example, a thrown exception after tier-up) still
    writes the partial report. The original execution error remains primary if
    writing that report also fails. A host command timeout can precede isolate
    report delivery; no empty artifact is fabricated in that case.
- JIT compile artifacts:
  - Default directory:
    `cargo run -p otter-cli -- --jit-artifacts run <file>`
  - Explicit directory:
    `cargo run -p otter-cli -- --jit-artifacts=/tmp/otter-jit-artifacts run <file>`
  - Combine `--jit-events=<path>` and `--jit-artifacts=<directory>`;
    `codeObjectId` joins a successful compile event to its bundle manifest.
  - The artifact target must not already exist. Under the cooperative
    single-writer contract, the CLI writes a private sibling and atomically
    renames the complete root into view. This is not crash-durable storage or
    a cross-process no-clobber primitive.
  - Each compile directory contains exact runtime-local `code.bin`,
    portable semantic `code-normalized.bin`, typed `relocations.json`,
    annotated ARM64 `asm.txt`, `bytecode.txt`, tier input, `code-map.json`,
    `safepoints.json`, and optimizer `deopt.json` when applicable.
  - Named property probes in Template and Graph use the VM's one shared
    property action table (`propertyActionCacheTable` relocation). Each
    receiver shape and atom key retains independent load and store facts,
    including an inherited holder slot and a different own addition slot.
    Template probes it after a CacheIR miss; Graph retains its current
    feedback admission. Live shape, descriptor, chain, bank-capacity and
    traced target-shape proofs precede effects. A miss completes the
    original source operation once through its committed runtime owner.
  - Template and Graph share one native global-binding guard and bank reader
    per architecture. `globalLexicalCell` relocations identify the exact source
    function and byte PC; reads use the live permanent traced cell. Eligible
    Graph reads use effectful `LoadGlobalBinding` nodes and preserve their full
    inlined source-frame chain. The active realm must match the source realm;
    global-object hits also prove the declarative epoch, live shape or dictionary
    layout, descriptor flags, and current slot capacity. A mismatch or TDZ hole
    exits before the read to the canonical source operation. Foreign-source
    proof baking and body inlining decline; committed cold operations enter the
    source realm, and a disposed source realm fails without an ambient fallback.
  - String literals are canonicalized before the compile snapshot and expose
    `stringConstantCell` relocations keyed by
    function id and byte PC. Exact addresses are redacted. Generated code reads
    the current value from an address-stable GC-traced cell; moving collection
    rewrites that cell in place, and no moving string handle is baked into code.
    If a cold literal cannot be materialized with active frame roots, optional
    compilation declines without publishing a code object or JavaScript error.
  - The optimizing tier (`crates/otter-jit/src/graph/`) builds a graph from
    bytecode and feedback. Every bytecode operation has a node; one without a
    specialized lowering runs its baseline operation as a Generic node, so a
    function is never declined as a whole. Checks leave by eager deopt to the
    instruction they guard. Calls may be inlined under a proof of the
    callee's identity; an exit inside an inlined body rebuilds the whole
    chain of interpreter frames. The tier emits native AArch64 and x86_64 code
    from one Graph, allocator and reconstruction model; unsupported targets
    run Template code only.
  - Native context/closure allocation uses the VM-owned `allocValue3` Probe
    boundary on both targets. Context, lexical `this` and `new.target` are
    explicit boxed operands; the innermost published source owns the callable
    constant. LAB fits publish only initialized cells. Collecting Graph misses
    restore updated canonical homes before rebuilding an interpreter frame.
    Focused runtime proof: `cargo test -p otter-runtime --test jit_native_context_closure`;
    it earns normal-policy native admission, observes fits, then arms moving GC
    in the same installed generation. Keep its exact source/inline/realm
    assertions when changing allocation lowering.
  - Every optimizing `optimized-ir.txt` starts with `; otter graph`, declares
    constants in node-id order, then lists the blocks in layout order: each
    phi and node with its inputs,
    representation and frame states. The code map has one `instruction`
    region per emitted node (`operation` is `v<id> <Kind>`, `operationIndex`
    the node id, `functionId` its owning source body, and `logicalPc`/`bytePc`
    its instruction in that body, including nested inline callees) and one
    `out-of-line` structural region for exits, slow paths and the return
    path.
  - `deopt.json` lists each exit's frames outermost first, with their resume
  PCs and value locations. `safepoints.json` gives, per point that may
  collect, the tagged locations the collector scans, the inlined frames it runs
  in (`inlineFrames`) and the outer call PC (`callPc`).
  Graph code uses one initialized tagged spill region and separate untagged
  homes. Collecting slow paths save control-flow-live registers into canonical
  homes; cold deopt exits materialize register-only values into those same homes.
  - Exact code may contain process addresses and is not a portable golden.
    Compare `code-normalized.bin` across processes; its relocation tokens and
    branch targets are symbolic, and it is not executable. `relocations.json`
    uses exact `code.bin` offsets but never serializes resolved addresses.
  - `asm.txt` starts with `; otter jit aarch64 assembly` and
    `; offset-basis=code.bin`. Native locations use `+0x<8-hex>:` offsets,
    local branch targets use `L<8-hex-offset>` labels, and baked address sites
    replace the address-bearing sequence with a symbolic `relocation …` line
    rather than printing its resolved value or immediate chunks.
    Words the decoder does not recognize remain visible through a `.word`
    fallback. Join a process-local profiler PC to `runtimeAddressRange`, then
    use assembly offsets and `code-map.json` to reach bytecode/tier operations.
    The hexadecimal range exists only under explicit artifact capture and is
    not portable or callable. `code-map.json` records actual `callEntryOffset`
    (null for a body without a private-JS entry) separately from the tier
    `entryOffset`; the manifest entry label describes the compile trigger.
    The owned generation snapshot's `call_entry_offset` and `current_entry`
    independently prove retained capability and current function-cell selection.
    Inspect `deopt.json` or `safepoints.json` as
    applicable. `safepoints.json` contains `{records, returnSites}`: each actual
    generated JS CALL/BLR return offset references the existing source/root
    record by `safepointId`. Join suspended callers through their exact retained
    code object; a tail branch adds no return association. Active C helpers
    continue to publish their current PC and collecting root recipe.
  - Template plain-call and method inlines expose `inlineCall*` /
    `inlineMethod*` guard, body, hit-epilogue, and deopt-teardown regions plus
    shared `inlineScratchSetup` and per-op `inlineInstruction` regions.
    `inlineSite` identifies the caller PC while top-level `functionId`
    identifies the callee. Inspect `inlineScratchLayout` for compact
    register/receiver slots and ordered argument/receiver/`undefined` entry
    values. A zero-width
    `inlineInstruction` is an intentionally coalesced operation, not missing
    capture.
  - Guarded `CallMethodValue` sites whose declared entry is a
    no-allocation leaf (String `indexOf`/`charCodeAt`/…, Map `get`/`has`,
    Set `has`) call the entry directly from Template code: the receiver and at
    most one argument sit in the leaf ABI registers, with no root record,
    safepoint, VM PC or status decode. An exotic receiver's pinned holder is
    proven through `JitCacheIrOp::LoadIntrinsicPrototype` and either its shape
    plus ordinary state or, in dictionary mode, an unchanged
    `dictionary_shape_id` (`JitCacheIrOp::GuardDictionaryLayout`), which is
    always the case for the String-wrapper `%String.prototype%`.
    `JitGuardedMethodCall::holder` (`Receiver` / `Shape` / `Dictionary`) is
    the one holder contract. A proof or leaf miss enters the committed method
    call once, without deopt or replay. The graph tier keeps such calls as
    their baseline operation.
  - Template AArch64 emits `nativeLeafCall` for explicit-receiver calls
    (`LoadProperty` + `CallWithThis`) whose loaded callee is a
    `jit_static_native` declaration. A declaration with `this_operand`
    (String search/charCode methods, Map `get`/`has`, Set `has`) receives
    `this` as its first word and proves it itself; plain calls and shaped
    method sites pass no receiver word and never lower it. Misses enter the
    ordinary call once, never a bail.
  - Both native Graph encoders lower exact-arity pure declarations at actual
    `CallWithThis` sites to `CheckNative` plus `NativeLeaf`. The two boxed
    operands use the existing heap/value/value C ABI. All live and eager-only
    values have canonical homes before this noncollecting call. A pre-effect
    miss resumes that exact call with its loaded callable, receiver and
    evaluated arguments; preceding property loads and argument work do not
    replay. Mutating, plain-call and guarded-holder sites retain their current
    paths. No leaf-hit safepoint, frame interop or source stamp is emitted.
  - `Interpreter::note_jit_optimized_bail_at` is the one owner of optimizing
    exits: an `Int32Overflow` / `NegativeZero` exit from `Add`, `Sub`, `Mul`,
    `Neg`, `Increment`, `AddImm` or `SubImm` (the innermost site for inlined
    deopts) widens that site's arithmetic feedback once and retires the
    generation, for entry, OSR, generated-linkage and inlined exits alike.
  - Leaf entries are
    described by `runtime_stubs::leaf_entry_shape`: the pure family (two
    words) and the in-place mutating families (two or three words, own write
    barrier), e.g. Map `set` overwriting an existing key; a mutating probe
    writes the heap and is never commoned or hoisted.
    Separately evaluated Map/Set named property loads use
    `JitCacheIrOp::LoadIntrinsicPrototype`: exact type and the existing clean
    instance latch select the pinned realm prototype, followed by ordinary
    live shape/descriptor/slot guards. Same-slot replacements are read live;
    accessors, shadows and prototype overrides use the committed cold load
    once. The generated hit has no allocation, runtime call or safepoint, and
    preserves the loaded callable across later argument evaluation. Primitive
    String named loads use the same proof plus
    `JitCacheIrOp::GuardDictionaryLayout` on `%String.prototype%`; `length`
    and index names never read the prototype. Every in-place descriptor change
    of a dictionary-mode object assigns a fresh `dictionary_shape_id`.
    Collection `size` retains its existing path.
  - Assembly decoding and formatting run only when `--jit-artifacts` is
    requested. The disabled path does not clone code, disassemble it, or build
    artifact text.
  - Full workflow and output-shape notes:
    `docs/site/src/content/docs/engine/jit-debugging.md`.
- Collection-service observation (default-off):
  - `Runtime::start_gc_pause_capture(capacity)` / `take_gc_pause_capture()`
    expose owned bounded records at a quiescent boundary.
  - `otter-resource-probe` (`otter-benchmark --features engine`) exports actual
    per-service Chrome Trace Event JSON after capture and separate forced-full
    retention component snapshots. Overflow/errors/split coverage are explicit.
  - Full service includes nested minor work and trigger accounting. Do not add
    existing full/minor cumulative counters or label them per-cycle tails.
  - Enforce watchdog/source/binary identities in the capture driver; use an idle
    native host. Service time is not whole allocation latency or a proven Bun
    whole-mutator comparison. See `docs/site/src/content/docs/engine/gc-observation.md`.
- CPU profile + folded flamegraph stacks:
  - `cargo run -p otter-cli -- run <file> --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Optional: `--cpu-prof-interval 1000 --cpu-prof-name my-run`
  - Produces both `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph.pl).
  - Stack samples are captured by an opt-in bytecode-dispatch sampler from live
    VM frames. Native JIT execution is currently a sampling blind spot.
  - The direct synchronous runtime used by `--cpu-prof` does not enforce
    `--timeout` yet; bound potentially hanging profiler runs externally.
  - Baseline overhead sanity check (`cpu-prof` should stay opt-in): compare `/usr/bin/time -p target/release/otter run <file>` vs `/usr/bin/time -p target/release/otter run --cpu-prof --cpu-prof-dir /tmp/otter-prof <file>` and watch `real` / `sys` delta.
  - Script args are forwarded to `process.argv`: `cargo run -p otter-cli -- run benchmarks/cpu/flamegraph.ts math 2`
  - Shorthand mode also forwards args: `cargo run -p otter-cli -- benchmarks/cpu/flamegraph.ts json 1`
- Test262 timeout triage:
  - `cargo run -p otter-test262 -- run --filter "<pattern>" --timeout 20000`
  - `--timeout` is milliseconds (maximum 30000). Timeouts are recorded in the
    result; failure/timeout trace capture is planned but not yet implemented.
- NativeScope::object_allocation_bytes reads the actual ordinary-object cell
  footprint from a current handle, without JS allocation or raw-header access.
- Embedder-only inspector APIs currently expose IC, shape-transition, frame,
  heap-summary, and Chrome `.heapsnapshot` snapshots. See the docs-site
  [Step Trace](docs/site/src/content/docs/engine/step-trace.md) page.

### Debug Workflows (engine improvement)

- Timeout/hang triage:
  - Bound the run with `--timeout <seconds>`.
  - Capture the current text step trace with `--trace=<path>`.
  - Correlate the final `pc`/opcode with `--dump-bytecode`.
  - Timeout ring-buffer dumps and trace filtering are roadmap items, not
    current CLI flags.
- CPU hotspot triage:
  - Capture profile: `cargo run -p otter-cli -- run benchmarks/cpu/flamegraph.ts <mode> <scale> --timeout 0 --cpu-prof --cpu-prof-dir /tmp/otter-prof`
  - Inspect `.cpuprofile` (DevTools/Speedscope) and `.folded` (inferno/flamegraph).
  - Treat JIT-heavy profiles as incomplete until native code-range
    symbolization lands; compare counters and wall time as supporting evidence.
  - Compare hottest frames before/after optimization patches; do not rely only
    on total runtime.
- Async/host-op tracing is not implemented yet. Do not document planned flags
  as available commands.

### Debug/Profiling Roadmap Rules

- If a patch adds or changes debug/profiling behavior, update:
  1. Runtime behavior (Rust code)
  2. CLI/API surface
  3. This `AGENTS.md` section when developer workflow changes
- Keep tooling default-off (minimal overhead unless explicitly enabled).
- Prefer machine-readable outputs (`.trace.json`, `.cpuprofile`,
  `.heapsnapshot`, `.folded`) over ad-hoc text when adding new tooling.
- Output compatibility is mandatory:
  - `*.trace.json` must follow Chrome Trace Event format (`traceEvents`) for DevTools/Perfetto.
  - `*.cpuprofile` must follow Chrome/V8 profile schema for DevTools/Speedscope.
  - `*.heapsnapshot` must follow Chrome heap snapshot schema for DevTools Memory.
  - `*.folded` must use standard folded stack format for flamegraph tools.
- Do not introduce Otter-only primary profiling formats when a standard format exists.

## Security Model

Capability-based, deny-by-default. All permission work belongs in the runtime integration layer:
- `fs_read`, `fs_write` - Path allowlists
- `net` - Host allowlists
- `env` - Variable allowlists with built-in deny patterns for secrets (AWS_*, *_SECRET*, etc.)
- `subprocess`, `ffi` - Boolean flags

Practical rules when adding/altering APIs:
- **Never bypass capabilities**; enforce checks in the Rust boundary and cover with tests.
- **Env access must stay isolated**: preserve default deny behavior and secret deny patterns.

## TypeScript / Types

- Otter `.d.ts` files live in `packages/otter-types/`, hand-written, one file per surface (`serve`, `sql`, `ffi`, `xml`, `globals`), tied together by `index.d.ts`.
- That directory is the only copy: no crate bundles types, and nothing generates them, so a runtime surface and its declaration change together.
- If you add a new global API or built-in module surface, update the corresponding `.d.ts` file(s).
- Type checking integration (tsgo) is still being re-enabled on the current runtime/compiler path.

## CLI Notes

- Default config file search: `otter.toml`, `otter.config.toml`, `.otterrc.toml` (walks up parent dirs).
- Permissions flags are additive/overriding: `--allow-read/--allow-write/--allow-net/--allow-env`, plus `--allow-run` and `--allow-all`.
- Direct run is supported: `cargo run -p otter-cli -- path/to/script.ts` (no `run` subcommand).
- Script argv forwarding is supported in both forms:
  - `cargo run -p otter-cli -- run path/to/script.ts arg1 arg2`
  - `cargo run -p otter-cli -- path/to/script.ts arg1 arg2`

## Benchmarks

- VM tests: `cargo test -p otter-vm`
- Runtime tests: `cargo test -p otter-runtime`
- Test262 conformance: `cargo test -p otter-test262`
- Focused engine harness:
  `cargo run --release -p otter-benchmark --features engine --bin otter-engine-benchmark -- <subcommand>`
  - Current subcommands are `call`, `idle-memory`, `jit-compile`, `kernel`,
    `memory`, and `module`.
  - `call`, `kernel`, and `module` require `--jit-tier=interpreter`,
    `--jit-tier=template`, or `--jit-tier=production-tiered`. `jit-compile`
    requires `--compile-tier=template` or `--compile-tier=optimizing` plus
    explicit numeric `--argument` values. `memory` is intrinsically
    interpreter/full-GC; `idle-memory` aggregates fresh release
    process/runtime samples across a controlled post-full-GC idle window. Do
    not infer a benchmark tier from legacy JIT environment variables.
  - `kernel` snapshots every `JitRuntimeStats` counter outside timed samples
    and emits its warmup-plus-measurement delta as an informational `jit-*`
    metric, including zeroes.
  - `module --runtime-reuse=fresh-per-sample` uses a new runtime for every
    warmup and measured execution, discarding warmup runtimes;
    `reused-across-samples` reuses one validated runtime. Runtime reuse is not
    a module-cache hit.
  - Every command emits the one live machine-readable result format. Format
    changes are hard breaking: update the runner, fixtures, tests, and
    `benchmarks/README.md` together; do not add compatibility readers or
    legacy output modes.
  - Failed, timed-out, unavailable, and unvalidated observations are
    non-scoreable and must remain visible. A dirty but validated observation
    may remain scoreable for local investigation, but is never
    baseline-eligible.
  - Engine fixtures live under `benchmarks/fixtures/engine/`.
- External suite runners and the baseline capture protocol are documented in
  `benchmarks/README.md`. Raw local results belong under the ignored
  `benchmarks/results/` directory.
- Retained-allocation GC diagnosis uses
  `cargo build --release -p otter-benchmark --features engine --bin otter-allocation-probe`
  followed by `target/release/otter-allocation-probe construct production 200000`
  (`spread`, `interpreter`, and `template` select the other cases). Configure
  `OTTER_GC_STRESS` separately for each process. The report includes existing
  GC/JIT timings, root-slot totals/peak, relocation updates, and receiver
  counters; full-GC time includes its minor collection, so those pause totals
  must not be added. `emit-kernel construct` or `emit-kernel spread` emits the
  same 200k workload for the standard engine benchmark; see `benchmarks/README.md`.
- Clean engine baseline capture:
  `cargo run --locked --release -p otter-benchmark --features engine --bin otter-engine-baseline -- capture`
  - The fixed matrix runs serially and keeps its outer watchdog in the
    unversioned capture manifest; successful engine records retain
    `sampling.timeoutMs: null`.
  - Publish only with the same binary's `publish --capture <ignored-dir>`
    command. It revalidates every record and creates the one current
    `benchmarks/baseline/` directory only after measurement is complete.
- Files under `benchmarks/archive/` are historical evidence only. Their old
  binary names, commands, tier labels, and data formats are unsupported and
  must not be presented as the current baseline.

## Test-Driven Workflow

When implementing features covered by Test262 or Node.js compatibility tests:

### 1. Establish Baseline
```bash
just test262-filter "FeatureName" 2>&1 | grep -E "(passed|failed|Pass rate)"
# Example output: "Pass rate: 39.0% (156/400)"
```

### 2. Fix by Failure Category
Prioritize fixes by impact:
1. Most common error type first (e.g., "TypeError: X is not a function")
2. Then edge cases and spec compliance details

### 3. Track Progress
After each fix, re-run and document the delta:
```bash
# Before: 39.0% (156/400)
# After:  42.4% (170/400)  ← +14 tests passing
```

### 4. Validate No Regressions
Run full test suite after changes to core modules:
```bash
cargo test -p otter-vm
cargo test -p otter-runtime
```

## Conformance Tracking

`ES_CONFORMANCE.md` tracks Test262 conformance by ECMAScript edition and by feature area.

### Before starting work

- Look up the relevant section in `ES_CONFORMANCE.md` for baseline pass rates
- Run the targeted test262 subset: `just test262-filter "Array/prototype/map"`

### After completing work

- Re-run tests and note the delta (before/after pass rates)
- If pass rate changed significantly, regenerate:

```bash
just test262-save && just test262-conformance
```

- Include before/after rates in commit message or PR description

### Timeout policy

All test262 runs use a 10-second per-test timeout (hardcoded fallback). Tests that hang
are recorded as `Timeout` in the conformance doc. If you encounter frequent timeouts in
a specific area, investigate for infinite loops before attempting other fixes.

## Key Files

- `ES_CONFORMANCE.md` - ECMAScript conformance status by edition and feature
