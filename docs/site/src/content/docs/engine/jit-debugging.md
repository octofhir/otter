---
title: "JIT Debugging"
---

Otter can capture two independent, default-off JIT diagnostic channels:

- structured events explain when and why compilation, inlining, OSR, side
  exits, and deoptimization happened;
- artifact bundles preserve the compiler input, exact native output, symbolic
  address sites, and a portable comparison stream for each successful compile.

Neither channel writes from the VM or JIT compiler. The engine returns bounded,
owned data to the outer runtime, and the CLI performs filesystem I/O only when
the corresponding flag is present.

## Capture a run

Build the release CLI, then run the production tier policy:

```sh
cargo build --release -p otter-cli

target/release/otter \
  --jit-events=jit-events.json \
  --jit-artifacts=jit-artifacts \
  run examples/jit_bench.js
```

Normal CLI execution always includes the optimizing tier with template
fallback. Use `--jitless` as the no-native-code oracle. Diagnostics remain
default-off in either mode.

`--jit-events` without a value defaults to `otter-jit-events.json`.
`--jit-artifacts` without a value defaults to `otter-jit-artifacts`. Both flags
also accept an explicit value with `=`. The artifact target must name a
directory that does not exist. Under the cooperative single-writer contract,
Otter writes a private sibling and renames the complete root into place. This
is atomic visibility, not crash-durable storage or cross-process locking.

On a JavaScript exception, Otter keeps the original runtime error primary and
best-effort persists every successful compile already captured. A host timeout
that fires before the isolate replies may have no partial batch to write.

## Structured events

`compilePrepared.globalLoadSites` counts all analyzed global reads.
`globalLexicalLoads` counts permanent global-declarative cells available for
direct generated reads, while `globalObjectLoads` counts guarded
global-object dictionary slots. `directCallees`, `directConstructs`,
`directMethodSites`, and `directMethodTargets` report stable function links
whose current generations were available for generated plain, base-construct,
and bounded polymorphic method linkage,
separately from `inlineCallees` /
`inlineMethods`, which count bodies offered to the inliner. A
`directCallPlan` event records every observed call target inspected.
`targetIndex` / `targetCount` identify its position in the bounded chain.
`callKind` is `plain`, `method`, `construct`, `derivedConstruct`,
`superConstruct`, or `derivedSuperConstruct`. Its typed result is either
`available`, with the planning-time code-object id, target tier, and
`thisMode` (`constructReceiver` or `derivedConstructor` for construct
planning), or
`rejected` with one of `missingCallee`, `ineligibleFunction`,
`methodGuardUnavailable`, or `noEntryGeneration`. Fresh callee-owned capture
cells no longer reject direct linkage: generated entry allocates them into a
caller-reserved upvalue spine and appends the closure's inherited cells before
publishing the callee frame.

For every available plan in a successful compile, `directCallLowered` records
the backend's actual choice: `generated`, `inlined`, or `rejected` because the
bounded stack layout is unsupported or the site was eliminated. It repeats
`targetIndex` / `targetCount`; a generated outcome repeats the target
generation and tier current when the caller compiled plus `thisMode`.
Generated code retains only the permanent function-cell link: later tier
publication switches the selected generation without recompiling the caller.
`callerCodeObjectId` identifies the exact successful caller generation.
Planning and lowering are separate events so diagnostics never claim a native
call edge that the backend did not emit.

Every direct-call region and entry-cell relocation reports
`ownUpvalueCount` and `inheritedUpvalueCount` alongside the register and stack
reservation fields. The counts describe the exact stack-owned spine contract
used by that edge. Portable normalized code retains both counts because they
change frame semantics; only the generation-local `targetCodeObjectId` is
excluded. Hot observed targets may be prepared to a bounded depth of two
edges, allowing a closure-backed callee's already-observed nested call to join
the same sealed generation without recursively compiling an unbounded call
graph.

Base-construct artifacts add `directConstructPrepare` between the shared guard
and frame publication. Its nested `directConstructPrepareFast` region probes an
already materialized own data `new.target.prototype`. Exact class wrappers then
enter `directConstructReceiverAllocFast`, which guards the live wrapper,
prototype chain, collector marking state, young from-space page, bump capacity,
and heap cap before initializing the complete object and accounting it in
generated code. `directConstructReceiverAllocCold` is the rooted allocator
sibling for a structural guard miss, nursery refill/GC, stress mode, or OOM.
For a conservatively matched straight-line base initializer, the immutable plan
reuses the VM's final hidden-class cache and creates undefined own slots up
front. Fixed, spread, and generated `superConstruct` bodies then overwrite
ordinary existing slots without a property-transition stub. This pre-shape is
admitted only when every initializer name is absent from the selected prototype
chain. `directConstructPrepareObservable` is the cold pre-effect-miss sibling
for accessors, proxies, bound functions, lazy or otherwise uncertain
prototypes; it owns the exact observable lookup. All paths root the receiver in
the Machine safepoint area and initialize the same stack-owned `NativeFrame`
used by plain and method calls. Derived construction
publishes a hole `this`, while `superConstruct` inherits the caller's live
`new.target`; `BindThisValue` commits the returned superclass receiver in both
stack-owned and materialized compiled activations. A later callee deopt resumes
the already-started construct and retains the base/derived return contract; it
never replays prototype lookup or `super(...)`.

Non-simple class-constructor fields that can append an own data slot remain at their
original bytecode position. `machineConstructorFieldTransition` identifies the
generated guard-and-commit region: it proves the receiver and complete ordinary
prototype chain, publishes the VM-interned child shape into pre-reserved
storage, stores the live value, and runs the required barriers. Allocation
caches only the immutable plan and reserved capacity; a guard miss deoptimizes
before the source `StoreProperty` starts. `machineClassSuperLoad` reads an exact
class wrapper's live superclass, `machineDerivedThisBindFast` commits the first
successful `super()` result, and `machineDerivedThisBindCold` owns the repeated
bind error. `directConstructResultFast` performs base substitution and valid
derived selection in generated code; `directConstructResultThrow` is the cold
primitive/uninitialized-`this` validator. Late tagged Machine roots cover fixed
arguments across receiver preparation, so the allocator's early and late homes
are not part of the call ABI.

Runtime and engine-benchmark snapshots expose exact receiver-allocation
attribution. `jit-receiver-alloc-attempts` equals generated successes plus
guard and space misses. Every guard/space miss owns one
`jit-receiver-alloc-cold-transitions` and one
`jit-receiver-alloc-rust-transitions`; the cold sibling separately attributes
GC transitions, page refills, and OOM. `jit-receiver-alloc-deopts` remains a
separate semantic refusal count and should stay zero for allocation misses,
which resume the already-selected construct instead of replaying it.

For every monomorphic body admitted by the optimizing tier's inline budget,
`inlineLowered` records the owning code object, parent/callee function ids,
parent logical and byte PCs, inline depth, and weighted cost. Its outcome is
`inlined` only after the complete CFG, SSA, frame-state, register-allocation,
and backend eligibility pipeline accepts the candidate subtree. A `rejected`
outcome owns the exact typed stage failure, so an opcode or frame shape the
splicer cannot represent is visible instead of silently filtered.

Exact artifacts represent a baked global-declarative cell as a
`globalLexicalCell` relocation keyed by byte PC. The raw pointer is redacted
from assembly and normalized code. Generated code reads the live cell value;
a TDZ hole still enters the canonical throwing global lookup.
Global-object records similarly retain only structural identity and the
property slot. Generated code proves the realm epoch and dictionary shape
before reading the live value; a mismatch uses the canonical lookup.

Ordinary `Op::Call` feedback uses one typed target population for bytecode
callees and static-native operations. When that population is monomorphic for
the original realm `Math.abs`, `compilePrepared.staticNativeCalls` counts the
site and `staticNativeCallPlan` reports target `mathAbs`. After a successful
compile, `staticNativeCallLowered` reports `callerCodeObjectId` and the
backend's actual result: `generated`, or `rejected` with
`arityUnsupported`, `layoutUnsupported`, or `eliminated`. A generated result
means the backend emitted an exact native function identity guard plus the
numeric leaf; it does not mean Rust was entered. Separate plan and lowering
events keep feedback selection distinct from emitted machine code.

`generatedCallDeopt` is emitted only when an already-started generated callee
bails into cold interpreter continuation. It records baked `callKind`, exact
`callerFunctionId`, `callerCodeObjectId`, `callerCallPc`,
`calleeFunctionId`, `calleeCodeObjectId`, `calleeTier`, `calleeResumePc`, and
`consecutiveDeopts`. Both code-object ids name retained exact generations, so
recompilation never merges unrelated edges or bodies. Capture remains
default-off and bounded; disabled hot calls construct no event.

Scalar query/coercion, static value-load, class-construction, and built-in
Array iterator opcodes share the VM's typed `RuntimeCall` boundary. Their
machine ABI still records the original logical opcode in `template-plan.txt`
and `code-map.json`, but the entry decodes it into a typed descriptor before
semantics begin. The descriptor reads and writes the published `NativeFrame`
window directly, regardless of whether an interpreter `Frame` also exists.
Consequently, a `generatedCallDeopt` at one of these opcodes now indicates a
real semantic refusal or another unsupported operation in the callee; it is
not an expected representation conversion. For supported scalar/load/class
operations, confirm `jit-generated-call-deopts == 0` and correlate the opcode's
code-map range with `jit-reentrant-stub-transitions`. A high transition count
with zero deopts means semantics stayed stack-owned but the operation itself
remains a hot candidate for Machine IR or a narrower leaf/alloc stub.

## Directory layout

The root contains the current index plus one directory per retained successful
compile:

```text
jit-artifacts/
  index.json
  jit-0000-template-f7-c11/
    manifest.json
    bytecode.txt
    template-plan.txt
    code.bin
    code-normalized.bin
    asm.txt
    code-map.json
    relocations.json
    safepoints.json
  jit-0001-optimizing-f9-c12/
    manifest.json
    bytecode.txt
    optimized-ir.txt
    code.bin
    code-normalized.bin
    asm.txt
    code-map.json
    relocations.json
    deopt.json
    safepoints.json
```

The suffixes identify capture order, tier, VM function id, and isolate-local
code-object id. `index.json` also reports retained bytes, dropped bundles,
dropped bytes, and whether the hard count or byte bound truncated the capture.

Every `manifest.json` records the Rust target triple, architecture, operating
system, tier, function and module identity, entry kind, bytecode size, code
size, and explicit `filesPresent` / `filesAbsent` inventories.

## Payloads

| File | Meaning |
| --- | --- |
| `bytecode.txt` | Deterministic logical-PC and encoded-byte-PC listing. |
| `template-plan.txt` | The already-built template lowering plan and its decoded operand side buffers. |
| `optimized-ir.txt` | Deterministic optimizing input: the legacy reverse-postorder unit or normalized Machine IR plus allocation for switched functions. |
| `code.bin` | Exact finalized executable bytes for this runtime process. |
| `code-normalized.bin` | Non-executable semantic instruction stream with symbolic relocations and logical branch targets. |
| `asm.txt` | Annotated AArch64 assembly over the exact bytes in `code.bin`. |
| `code-map.json` | Native offset ranges correlated with bytecode/tier operations, structural regions, and OSR entries. |
| `relocations.json` | Typed runtime-local address sites and their exact `code.bin` ranges, without resolved address values. |
| `deopt.json` | Optimizer frame reconstruction metadata; omitted for template code. |
| `safepoints.json` | Tagged frame/register/spill locations known to moving GC. |

`code.bin` is intentionally marked runtime-local. It can contain baked process
addresses and can differ across identical runs because of ASLR. Do not use it
as a portable golden file.

All native locations are offsets into the matching `code.bin`, never absolute
executable addresses. A range must satisfy
`0 <= startOffset <= endOffset <= manifest.codeBytes`.

`code-map.json` contains typed structural regions and validates every native
range against the matching code object.

Inspect the first line of `optimized-ir.txt` before interpreting the payload.
The replacement scalar-function path starts with `; backend=otter-machine-ir
scalar-function`, followed by normalized Machine IR and exact regalloc2 output;
its `code-map.json` owns one `machineScalarFunction` structural region. Functions
not yet switched retain the legacy optimized-unit banner. Both are payloads of
the one current artifact bundle, not separate artifact formats.

### Template leaf-inline regions

A template-tier `Call` or `MethodCall` may contain nested regions that expose
a deopt-safe spliced leaf:

| Region kind | Meaning |
| --- | --- |
| `inlineCallGuard` | Plain-callee function-id, closure type, and runtime-setup-state guards. |
| `inlineMethodGuard` | Receiver shape, holder, method identity, and bound-state guards. |
| `inlineScratchSetup` | Compact stack allocation and live entry-value materialization. |
| `inlineInstruction` | Exact native range for one callee operation. |
| `inlineCallBody` | Aggregate plain-callee range containing all `inlineInstruction` ranges. |
| `inlineMethodBody` | Aggregate range containing all `inlineInstruction` ranges. |
| `inlineCallHitEpilogue` | Plain-call scratch release, result publication, and jump to call completion. |
| `inlineMethodHitEpilogue` | Scratch release, result publication, and jump to call completion. |
| `inlineCallDeoptTeardown` | Plain-call scratch release before exact caller deoptimization. |
| `inlineMethodDeoptTeardown` | Method-call scratch release before exact caller deoptimization. |

All these regions carry the same `inlineSite`:

```json
{
  "callerFunctionId": 438,
  "logicalPc": 12,
  "bytePc": 47,
  "hasReceiverProperty": true
}
```

The region's top-level `functionId` is the inlined callee. The `inlineSite`
identifies the caller `Call` or `MethodCall`; its logical and encoded PCs join
back to the enclosing caller instruction, whose `operation` distinguishes the
two forms even on shared scratch/instruction regions. Plain calls always
publish `hasReceiverProperty: false` and `receiverSlot: null`. Each
`inlineInstruction` uses callee-local `logicalPc`, `bytePc`, and dense
`operationIndex` values starting at zero. A coalesced `Move` or `LoadThis` may
have `startOffset == endOffset`: the operation remains inspectable even when it
emits no machine instruction.

`inlineScratchSetup.inlineScratchLayout` describes the complete compact
assignment:

```json
{
  "parameterCount": 1,
  "virtualRegisterCount": 6,
  "scratchSlotCount": 2,
  "slotBytes": 8,
  "stackAlignmentBytes": 16,
  "scratchBytes": 16,
  "offsetBasis": "postAllocationSp",
  "registerSlots": [0, 0, 1, 0, 1, null],
  "receiverSlot": 1,
  "entryValues": [
    { "kind": "argument", "argument": 0, "register": 0, "slot": 0 },
    { "kind": "receiver", "slot": 1 }
  ]
}
```

`registerSlots` is indexed by callee virtual register; `null` means the
register is unused. `entryValues` is ordered arguments, receiver, then
function-entry `undefined` locals. Its third typed form is
`{"kind":"undefined","register":<r>,"slot":<s>}`. Every slot offset is relative
to `sp` after allocation, and
`scratchBytes = align_up(scratchSlotCount * slotBytes, stackAlignmentBytes)`.
Distinct virtual registers may share a slot only when their live ranges do not
overlap under source-read-before-destination-write semantics.

Ranges reveal both control paths without claiming which one executed:
guard precedes setup, setup precedes body, the body contains every inline
instruction, and the hit epilogue begins at body end. Body misses pass through
the matching `inlineCallDeoptTeardown` or `inlineMethodDeoptTeardown`; early
guard misses skip teardown and branch directly to the same exact caller side
exit. No path replays an already-started inline body.

### Compiler-generated call regions

A monomorphic non-inlined plain call, method call, constructor, or spread
call-family operation may contain these generated-linkage regions:

| Region kind | Meaning |
| --- | --- |
| `directMethodGuard` | Method only: receiver shape, prototype chain, method slot, callable identity, and closure-state guards. |
| `directCallGuard` | Capacity, remaining callable state, and stack-budget guards. |
| `directCallFrameSetup` | Rooted stack register initialization, entry-cell lease, and native-frame publication. |
| `directCallNativeEntry` | Direct branch-and-link to the acquired native entry. |
| `directCallReturn` | Native status handling and cold callee-deopt entry when required. |
| `directCallCleanup` | Caller publication restore, activation retirement, lease release, and accounting unwind. |
| `directCallEntryReject` | Pre-entry lease rollback and accounting unwind before exact caller deoptimization. |

Each direct-call region keeps caller `functionId`, `logicalPc`, and `bytePc`
and carries one typed `directCall` object:

```json
{
  "callKind": "method",
  "targetFunctionId": 11,
  "targetCodeObjectId": 29,
  "targetTier": "template",
  "thisMode": "methodReceiver",
  "argumentMode": "fixed",
  "calleeNativeFrameBytes": 160,
  "linkageBytes": 112,
  "reservedStackBytes": 272,
  "calleeRegisterCount": 6
}
```

`callKind` is `plain`, `method`, `construct`, `derivedConstruct`,
`superConstruct`, or `derivedSuperConstruct`. `targetCodeObjectId`,
`targetTier`, and `calleeNativeFrameBytes` describe the generation current
when this caller compiled; they are planning diagnostics, not a permanently
baked dispatch target. `thisMode` records the call binding emitted before
frame publication. `argumentMode` is `fixed` or `spread`. A spread operation
keeps its source array rooted across cold resolution and receiver preparation,
then copies the declared-parameter prefix directly into the same unpublished
callee frame with a leaf/no-allocation runtime stub. It does not introduce a
second frame, call ABI, result transition, or replay path.

Compiler-generated spread wrappers normally contain the frontend's canonical
`GetIterator` / `IteratorNext` / `ArrayPush` collection loop before the call.
When its source is an ordinary Array whose `@@iterator` is still the original
`Array.prototype.values`, those operations run through the same published
stack-owned activation and the generated wrapper can return without an entry
deopt. An own iterator, prototype replacement, accessor, or non-Array source
misses before observable work and resumes the materialized opcode path. A
generated-call deopt at that guard is therefore a semantic fallback, while
repeated deopts at the default Array collection PCs indicate a regression.
`linkageBytes` is the exact caller-owned `NativeFrame`, tagged register window,
bookkeeping, and alignment. `reservedStackBytes` is the planning-time sum with
the captured target prologue. At runtime the permanent function cell selects
the current generation, and generated linkage reads that generation's actual
code-object id, tier, and native-frame reservation before entry. The same
planning object appears on the stable function-cell relocation in
`relocations.json`. `asm.txt` renders all fields on region annotations and the
`directCallEntryCell(...)` pseudo-line. No artifact serializes the cell's
process-local address outside exact runtime-local `code.bin`; metadata and
portable code remain address-free.

`directMethodGuard` also carries a typed `methodGuard` object with
`receiverRegister`, `methodFunctionId`, `receiverShape`, ordered
`prototypeShapes`, and `methodValueByte`. Guard, capacity, or invalidation
failure deoptimizes the original caller opcode before effects. A bailout after
native entry resumes the published callee through cold deoptimization and
never invokes the call again. Its `generatedCallDeopt` event joins exact caller
and callee generations to the interpreter resume PC.

### Guarded static-native call regions

A generated `Math.abs` ordinary-call leaf contains two structural regions:

| Region kind | Meaning |
| --- | --- |
| `staticNativeCallGuard` | Callable type and exact original bootstrap-function identity checks. |
| `staticNativeCallBody` | Numeric `Math.abs` machine-code leaf; no Rust/native call boundary. |

Both carry caller `functionId`, `logicalPc`, `bytePc`, and
`staticNativeCall: "mathAbs"`. Guard or numeric-domain failure deoptimizes the
original `Call` before effects.

The identity materialization appears in `relocations.json` as the typed,
address-free target
`{"kind":"staticNativeBuiltinFunction","target":"mathAbs","bytePc":<pc>}`.
`code-normalized.bin` retains that semantic target and byte PC, while exact
address-bearing machine bytes remain only in runtime-local `code.bin`.

These regions and relocation records are captured only when
`--jit-artifacts` is requested. The same generated leaf runs without building
artifact DTOs when capture is disabled.

## Annotated ARM64 assembly

`asm.txt` is ordinary UTF-8 assembly text with one current two-line header:

```text
; otter jit aarch64 assembly
; offset-basis=code.bin
```

The banner identifies the listing kind. Every rendered instruction or
relocation range starts with `+0x<8-hex>:`, measured from byte zero of the
sibling `code.bin`. A relocation range is deliberately rendered as one
symbolic line; intermediate MOV-wide instruction offsets stay hidden with
their address chunks. Local branch destinations are rendered as
`L<8-hex-offset>` labels, so a branch can be followed without exposing a
process address. If the built-in decoder does not recognize a four-byte
instruction, the exact word remains visible as `.word 0x<8-hex>`; one unknown
instruction therefore does not make the remainder of the artifact unavailable.

The remaining header comments record target, architecture, operating system,
tier, function/module/code-object identity, compile target, entry offset,
exact code length, deopt summaries, and the safepoint inventory. Body lines
use these stable forms:

```text
  ; region kind=<kind> range=+0x<start>..+0x<end> ... pc=<pc> byte-pc=<byte-pc> tier-op="<operation>"
  ; region kind=inlineScratchSetup ... inline-site=caller:<function>:pc:<pc>:byte:<byte-pc> receiver-property=<bool> parameters=<n> virtual-registers=<n> scratch-slots=<n> slot-bytes=8 stack-alignment=16 scratch-bytes=<n> offset-basis=postAllocationSp register-slots=[...] receiver-slot=<slot|-> entry-values=[...]
  ; region kind=directCallNativeEntry ... call-target-function=<id> call-target-code-object-id=<id> call-target-tier=<tier> call-this-mode=<mode> call-argument-mode=<fixed|spread> call-callee-native-frame-bytes=<n> call-linkage-bytes=<n> call-reserved-stack-bytes=<n> call-callee-register-count=<n>
L<8-hex-offset>:
+0x<8-hex>: <8-hex-word>  <decoded instruction or .word fallback>
+0x<8-hex>: relocation <register>, <symbolic target> ; encoded-bytes=<n> redacted
```

Comments are derived from metadata the compiler already owns. They identify
the overlapping `code-map.json` region and, when applicable, its function,
logical/encoded bytecode PC, template operation or optimized-IR operation,
structural block/edge/backend glue, OSR entry, or deopt exit. A baked address
load is replaced by one offset-bearing `relocation …` pseudo-line with the
typed symbolic target from `relocations.json`; its resolved pointer and
immediate chunks are deliberately redacted. Instruction annotations use stable
`pc=` and `tier-op=` fields for bytecode/tier correlation; explicit backend
glue ranges prevent unattributed holes without inventing a source PC. Use
`code.bin` when exact executable bytes matter and `code-normalized.bin` for
portable cross-process comparisons.

All joins use the same `code.bin` offset basis:

- `code-map.json` maps assembly offsets and ranges back to bytecode and tier
  operations. During explicit artifact capture it also records
  `runtimeAddressRange` as hexadecimal text, allowing a process-local native
  program counter to join the owning code object before applying offsets;
- `relocations.json` describes the symbolic meaning of baked-address ranges;
- `deopt.json` supplies frame reconstruction for a `deoptExitId` named by the
  code map or assembly annotation;
- `safepoints.json` supplies tagged frame/register/spill locations by
  safepoint id and frame state.

Safepoint records currently serialize `nativeReturnOffset: null`. Do not infer
an exact call-return instruction from assembly proximity; direct
safepoint-to-native-return correlation remains a follow-up. The corresponding
assembly summary says `native-offset=unavailable` while still preserving the
safepoint id, frame state, and tagged-location inventory.

Assembly generation is part of explicit artifact capture. Without
`--jit-artifacts`, compilation does not clone finalized code for diagnostics,
run the decoder, format assembly, or perform artifact filesystem I/O.

## Portable code comparisons

`relocations.json` uses `offsetBasis: "code.bin"`. Each sorted record describes the exact
`MOVZ`/`MOVK` range, destination register, emitted chunk shape, and a typed
symbolic target such as a runtime-stub descriptor, call trampoline, GC cage
base, property IC cell, or code-owned operand slice. Chunk immediates and
resolved pointer values are deliberately absent. Typed targets use camel-case
fields consistently. A direct-call entry-cell target additionally carries the
planning-time `directCall` generation/layout object shown above while the
relocation itself denotes the permanent function cell.

`code-normalized.bin` starts with the `OTJNCODE` marker, architecture id, and
logical-item count. Its typed `directCallEntryCell` token
contains target tier, `thisMode`, `argumentMode`, and stack/register layout while deliberately
omitting generation-local `targetCodeObjectId`, so otherwise identical
recompilations normalize equally. It is a semantic comparison stream, not
ARM64 executable code:

- a one-to-four instruction address load becomes one symbolic relocation
  token;
- local branch displacements become logical item targets, so ASLR-driven
  changes in address-load length do not move the comparison target;
- ordinary non-PC-relative instructions retain their exact instruction word.

Match bundles by module, function, tier, and entry, then compare their
normalized streams. Continue to use `code.bin` offsets when joining the code
map, relocation records, OSR entries, deopt exits, or safepoints.
`runtimeAddressRange` is intentionally process-local profiler metadata and
must not be used for cross-process comparison or as an executable pointer.

## Correlate an execution

Use this order when a hot function produces a wrong result or unexpected
fallback:

1. Re-run with `--jitless` to establish the bytecode oracle.
2. Capture `--jit-events` and find the function's `compilePrepared`,
   call plan/final-lowering events, `compileFinished`, OSR, bail, or deopt
   records. For a static-native site, compare `staticNativeCallPlan` with
   `staticNativeCallLowered` before inspecting artifacts.
3. Join a successful `compileFinished` to `manifest.json` by `codeObjectId`.
4. Read `bytecode.txt` and the first line of the tier input to identify the
   backend and logical operation.
5. Use `code-map.json` to map its logical PC and encoded byte PC to the exact
   native byte range.
   For a template call/method inline, first identify the caller through
   `inlineSite`, then inspect its guard, compact scratch assignment,
   callee-local instructions, and separate hit/deopt-teardown ranges. For a
   generated plain call, join caller and callee through
   `directCall.targetFunctionId`, treat `directCall.targetCodeObjectId` as the
   compile-time generation snapshot, then inspect its guard, stable-cell
   selection, setup, native-entry, return, cleanup, and entry-reject regions.
   For a static-native call, inspect
   `staticNativeCallGuard` and `staticNativeCallBody`, then join the guard's
   function identity through its `staticNativeBuiltinFunction` relocation.
6. Open `asm.txt` at the matching `+0x<8-hex>:` offset to inspect the emitted
   instructions and local branch labels.
7. Inspect `relocations.json` when the range materializes a runtime-local
   address.
8. Inspect `deopt.json` and `safepoints.json` when the range crosses a deopt or
   allocation boundary.

The interpreter [step trace](/otter/engine/step-trace/) complements this
capture: it shows the warmup and the last interpreter-visible PC, while the
artifact bundle explains the native body entered after that point.

## Embedding

Embedders request either channel explicitly:

```rust
use otter_runtime::{JitDebugRequest, Runtime, SourceInput};

let request = JitDebugRequest::disabled()
    .with_events(true)
    .with_artifacts(true);
let mut runtime = Runtime::builder()
    .jit_debug(request)
    .build()?;

let mut result = runtime.run_script(
    SourceInput::from_javascript("function hot() { return 42; } hot();"),
    "main.js",
)?;
let events = result.take_jit_debug_report();
let artifacts = result.take_jit_artifacts();
```

For abrupt completion, use `run_script_with_diagnostics` and inspect
`ExecutionAttempt::jit_debug_report()` plus
`ExecutionAttempt::jit_artifacts()`. Returned reports and bundles own all
strings and bytes; they contain no GC handle, executable pointer, isolate
borrow, lock, TLS state, or runtime registry reference, so they remain valid
after full GC and later JIT compilation.
