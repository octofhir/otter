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

## Focused iteration

Use `just quick [calls|gc|math|native|properties|all]` for incremental debug
checks. The default `calls` family runs the two constructor moving-GC tests;
`gc` checks scavenging, promotion preflight and remembered arrays; `math` checks
guarded Math methods; `native` checks resolved calls, evaluation order and moving
argument roots; `properties` checks named-slot proofs and shared-cache loads.
`all` runs every focused family. An unset `OTTER_GC_STRESS` defaults to 1;
explicit values and the Cargo target/runner configuration are preserved.
These checks support iteration. The full gate and required targeted Test262
comparison remain the closing evidence; cold compilation can dominate a quick run.

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
fallback. `--jitless` runs the template baseline tier without optimizing
compilation; `--interpreter` runs the bytecode interpreter alone and is the
no-native-code oracle. Diagnostics remain default-off in every mode. The
optimizing tier emits native code for AArch64 and x86_64 from the same Graph,
allocation plans and frame reconstruction recipes. Each target owns instruction
encoding and its register contract.

Own-field Graph operations carry a `FieldLocation` with its storage bank and
relative index. `LoadOwnField` and `StoreOwnField` keep address resolution inside
the emitted access: inline fields use a direct receiver-relative instruction;
suffix fields reread the live slab handle. The shape owns inline capacity, so
growing the suffix preserves the inline prefix. No suffix address is an SSA
value retained across a collecting operation. Stores retain the receiver and
value for the separate write barrier. The IR and banked CacheIR artifacts expose
this location; equal byte displacements in different banks identify different
fields.

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

`compileFinished` reports the compiler-hook result and retains duration/queue
delay as diagnostics only. A successfully compiled mapping can still fail
canonical publication: `installDeclined` identifies the exact function,
`codeObjectId`, tier and typed `invalidCode` or `resourceBudget` reason. Resource
refusals include the full centrally computed `requiredBytes` and current
`availableBytes`; the event is emitted only for an actual post-hook refusal.
Tier admission uses actually entered source-opcode attempts, immutable compiler
geometry, compiler attempt counts, and retained-code admission headroom.
Interpreter dispatch charges one attempt; Template code batches exact source
prefixes and flushes before reentrant helpers. Expanded lowerings count once per
source opcode, fused preflight counts zero, and a failed native guard followed by
interpreter replay counts both entered attempts. Graph code does not add work
because it is the highest tier. `compilePrepared.sourceWork` records the exact
visible total, rather than entry or static-span estimates.

Feedback epochs own stable-work origins; generated promotion targets name that
origin plus outstanding required work. Actual invoked compiler hooks charge an
attempt even if unsupported; pre-hook deferral does not. The fixed 64 MiB isolate
cap covers requested mapping capacity and owned generation payloads, including
active invalid mappings and permanent entry-cell tombstones, through existing
`GeneratedCodeBytes` leases. This bound excludes shared source/GC allocations,
registry directory/bucket storage and allocator bookkeeping; it is not an RSS cap. A lower host account
limit also constrains admission. The static code-byte estimate is a precompile
check; canonical registration reserves finalized code, copied dependencies,
root box and entry-cell payload before publication. Same-epoch retries remember
the exact resource demand returned by that admission boundary. Resource denial
disables the native wakeup until a later cold policy decision sees sufficient
headroom; retirement can admit unchanged actual work.
Host load and elapsed compilation time do not change promotion or retry
decisions; disabled event capture reads no compilation clock.

Property feedback distinguishes an unattempted site from an executed site whose
receivers admit no cache program. Accessors, proxies, mapped arguments, and
throwing property operations therefore remain ordinary runtime probes after
warming instead of repeatedly leaving for `InsufficientFeedback`. The IC
inspector exposes the latter state as `Uncacheable` until a program attaches.

`propertyStoreRuntime` aggregates generated named-store runtime entries by
`functionId`, logical `instructionPc`, selected `path`, `failed`, and
`nativeWay`. `propertyName` is the owned executable spelling; `count` is the
number of matching observations in this capture batch. Paths distinguish an
installed cache recipe, existing-slot installation, transition capture,
canonical Set semantics, uncached data assignment, and unsupported receivers.
`failed` records an error return, including throws after prior effects;
`nativeWay` means the runtime offered a native IC program, not that a caller
installed or reused it. Fully generated hits never enter this counter.

A counter occupies one event slot at its first observation and updates in
place. It is an aggregate, not a chronological per-call event. The shared
16,384-event cap still applies: existing counters continue updating when full,
while unseen site/path observations increase `droppedEvents` without building
names or growing the index. Drain, reset, and disabling capture clear the
batch-local indices. Capture remains default-off and allocates no names or
counter storage while disabled. These counters identify hot runtime sites;
a missing native way alone does not identify its exact lowering rejection.

`compilePrepared` summarizes the snapshot a compile consumes.
`globalLexicalLoads` counts global lexical reads carrying one permanent
direct-cell target, and `globalObjectLoads` counts global-object reads carrying
one epoch-and-shape guarded own slot. `literalCells` counts eagerly
prepared stable traced string and BigInt literal cells available as relocation loads.
`directCallees`, `directConstructs`,
`directMethodSites`, and `directMethodTargets` report stable function links
whose current generations were available for generated plain, base-construct,
and bounded polymorphic method linkage,
separately from `inlineCallees` /
`inlineMethods`, which count bodies offered to the inliner. A
`directCallPlan` event records every observed call target inspected.
`targetIndex` / `targetCount` identify its position in the bounded chain.
Graph `inlineLowered` events identify bodies actually accepted by the completed
graph, with their exact owning code object, source parent, callee, depth and
encoded-bytecode-byte budget charge. This includes nested bodies and bodies
without exits. VM bake refusals are `inlineCandidate` events; the current Graph
backend does not emit its later splice refusals. Template direct-call lowering
continues to report its typed final outcomes.
`callKind` is `plain`, `method`, `construct`, `derivedConstruct`,
`superConstruct`, or `derivedSuperConstruct`. Its typed result is either
`available`, with the planning-time code-object id, target tier, and
`thisMode` (`constructReceiver` or `derivedConstructor` for construct
planning), or
`rejected` with one of `missingCallee`, `ineligibleFunction`,
`methodGuardUnavailable`, or `noEntryGeneration`. A `noEntryGeneration`
target, or a target a baseline body keeps reaching through the generic call
boundary once its executions repay a direct-call-target compile, becomes a
pending target of that body. Entry refresh rebuilds a pending body at function
entry; a running baseline loop relinks at its next back-edge instead: the VM
compiles the pending targets, unlinks the body, and the poll resumes the
interpreter at the loop header (`Interrupt`/`Resume` side exit), whose next
OSR lowers the sites as generated calls. Installing a pending target's entry
code shortens the current back-edge poll window to one back-edge, and the
checkpoint charges exactly the back-edges consumed. An OSR compile also counts
the triggering loop's observed trip count as execution evidence for direct
targets inside that loop.

For every available plan in a successful Template compile, `directCallLowered`
records the backend's actual choice: `generated`, `inlined`, or `rejected`
because the backend has no proven linkage for this call shape, the bounded stack
layout is unsupported, the backend cost model prefers the smaller canonical
transition, or the site was eliminated. The corresponding typed reasons are
`backendUnsupported`, `layoutUnsupported`, `unprofitable`, and `eliminated`.

Template on AArch64 and x86_64 emits bounded method guard chains that read the
live callable and enter its current generation through the permanent entry
cell. A whole-chain miss commits method resolution once. Ordinary constructor
links use the constructor plan table and retain base, derived and super call
semantics. Spread construction uses runtime staging; its final event retains
the appropriate constructor kind and reports `backendUnsupported` for the
unimplemented direct link.
The x86 Template method path currently resolves the method and enters the
generic call ABI; it reports `backendUnsupported` for available method plans,
without claiming a proven function-cell edge. Reached call sites are never
reported as eliminated merely because their lowering uses the canonical path.
It repeats
`targetIndex` / `targetCount`; a generated outcome repeats the target
generation and tier current when the caller compiled plus `thisMode`.
Generated code retains only the permanent function-cell link: later tier
publication switches the selected generation without recompiling the caller.
`callerCodeObjectId` identifies the exact successful caller generation.
Planning and lowering are separate events so diagnostics never claim a native
call edge that the backend did not emit. Optimizing compiles emit no lowering
events; their call decisions are visible in `optimized-ir.txt`.

`inlineCandidate` records each plain-call inline candidate the VM offered to a
compile or rejected while baking its snapshot. `bakeRejection` names the
bake-stage reason; an offered body may still be declined by the compiler. When
an optimizing exit leaves from an inlined body, deoptimization rebuilds one
interpreter frame per inlined call, and each materialized frame records an
`inlineDeoptFrame` event with its chain `index`, the chain's `total`, its
`functionId`, and its `resumePc`.

Literal allocation relocations `jit_new_object` and `jit_new_array` use the
shared `reentrantValueSpan` ABI and a committed result pair. Despite the physical
signature's name, these allocation descriptors do not permit JavaScript
reentry. Object allocation consumes an empty span; array allocation consumes
boxed elements, preserving holes. The runtime copies the span before collection
and uses the canonical VM allocator. Template publishes its frame window as
roots and commits the returned value directly to the destination. Empty spans
use no packet storage, and literal allocation cannot deopt/replay after
entering the canonical boundary. Source register indices and array-operand
metadata addresses do not cross this ABI.

Lexical allocations use the existing VM-owned `allocValue3` ABI and its Probe
result domain on both native targets. `MakeFunction` passes three `undefined`
words; `MakeClosure` passes context, lexical `this` and lexical `new.target`.
`CreateContext` passes its parent plus boxed source-function and scope IDs;
`CopyContext` passes its source plus two `undefined` words. The published
safepoint identifies the innermost semantic source, including an inlined body,
whose original constant owns the callable template. No register indices,
metadata addresses or physical-frame lexical-binding reads cross this entry.

Prepared LAB fits initialize every payload word before publishing one fresh
cell. Context copies retain the original scope and parent while giving each
iteration its own slots. Arrow closures capture the semantic activation's
receiver and target; an ordinary factory inlined into a constructor captures
the factory's `this` and `undefined` target. A constructor-local arrow captures
that constructor's target. An unsupported callable kind uses the same typed
VM allocation kernel as the interpreter.

A collecting Probe roots its three operands and canonical tagged homes.
Graph restores collector-updated live registers before decoding a Miss/OOM
exit, so rebuilding frames cannot overwrite those homes with stale registers.
Success commits the result after the packet is released; Miss/OOM resumes
before the original allocation. The Probe domain permits no JavaScript throw
or reentry, and an illegal result status leaves through the fatal boundary.
The compiled-code ABI remains private engine plumbing; host extensions build
values through `NativeCtx` and handle scopes.

A construct site's receiver-family feedback is monotonic: provisional
receivers leave it unseen, one finalized family enables an exact guarded plan,
and a later different or provisional family permanently selects canonical
receiver preparation for future compilations. Alternating closures sharing a
bytecode body do not keep changing the feedback epoch. Existing plans still
guard the live family, immutable root and prototype proof before allocation.
Constructor stores use executable property CacheIR and the shared transition
table; receiver shape preparation does not publish a separate store program or
invalidate the constructor generation.

Runtime and engine-benchmark snapshots expose exact receiver-allocation
attribution. `jit-receiver-alloc-attempts` equals generated successes plus
guard and space misses. Every guard/space miss owns one
`jit-receiver-alloc-cold-transitions` and one
`jit-receiver-alloc-rust-transitions`; the rooted cold path separately
attributes GC transitions, page refills, and OOM. `jit-receiver-alloc-deopts`
remains a separate semantic refusal count and should stay zero for allocation
misses, which resume the already-selected construct instead of replaying it.

Exact artifacts represent a baked global-declarative cell as a
`globalLexicalCell` relocation keyed by function id and byte PC. The raw
pointer is redacted from assembly and normalized code. Generated code reads the
live cell value; a TDZ hole still enters the canonical throwing global lookup.
Global-object records similarly retain only structural identity and the
property slot. Generated code proves the realm epoch and dictionary shape
before reading the live value; a mismatch uses the canonical lookup.
Template exposes binding accesses through `templateBindingGuard`,
`templateBindingHit`, `templateBindingCold`, and `templateBindingJoin`
code-map regions, and global declarations through
`templateGlobalDeclarationCold` / `templateGlobalDeclarationJoin`. Generated
hits allocate nowhere. Guard, TDZ, const, accessor, Proxy, or unresolved misses
enter one committed cold call; they never deoptimize and replay the source
operation.

Int32-specialized `Add`, `Sub`, `Mul`, `Neg`, `Increment`, `AddImm` and
`SubImm` sites exit with `int32Overflow` or `negativeZero` when a result leaves
Int32. The VM records every optimizing exit's reason at its PC and widens the
site's arithmetic feedback past the refuted speculation, so the next compile
gives that site Float64 arithmetic. A trace therefore shows one such `bail` per
site followed by a recompile. Repeated identical `negativeZero` or
`int32Overflow` bails at one site indicate a missing repair. The
`arith-exit-repair` kernel pins this contract.

Every `LoadString` site exposes a `stringConstantCell` relocation keyed by
function id and byte PC. The process address is redacted. The cell is rooted,
address-stable across cell-table growth, and rewritten in place by moving GC;
generated code reads the current `Value` and never embeds the moving string
handle. Cold literals are canonicalized before the compile snapshot; failure to do so
declines optional compilation. Every published cell is therefore a direct leaf
load with no deoptimization, runtime fill, or recompilation loop.

Ordinary `Op::Call` feedback uses one typed target population for bytecode
callees, exact static-native leaves and general NativeFunction kind.
`compilePrepared.nativeCalls` counts both native plan forms. A general native
plan proves the full live cell kind before entering `jit_call_native`; the sole
Host kernel still reads its current callback, policy, captures, realm and
constructability. A changed native body remains a legal kind hit; a different
kind takes the canonical generic call once. Staged spread and exposed-arguments
forwarding retain their existing C trampoline entry. When the population is monomorphic for
a declared leaf builtin such as the original realm `Math.abs`,
`staticNativeCallPlan`
reports its target (`math_abs_leaf`). After a successful Template compile,
`staticNativeCallLowered` reports `callerCodeObjectId` and the backend's actual
result: `generated`, or `rejected` with `arityUnsupported`,
`layoutUnsupported`, or `eliminated`. A generated result means the backend
emitted an exact native function identity guard plus a call of the declared
leaf (see [Native leaf calls](#native-leaf-calls)). Separate plan and lowering
events keep feedback selection distinct from emitted machine code.

Exception handlers are a static table on each function: a range of
instructions, the handler's PC, and the register that receives the thrown
value. A `finally` block is ordinary code entered with a completion token, so no
tier keeps handler or completion state at run time. A committed getter or
callee throw in generated code selects the handler covering the published PC,
writes its exception register, and exits at the handler PC; the source operation
is not replayed, and exact deoptimization has no handler state to rebuild.
Function metadata resolves through its owning context, including when a later
script calls an earlier generated function.

`enteredGenerationDeopt` is emitted when a generation entered through the call
trampoline takes a side exit. It records `calleeFunctionId`, the exact
`calleeCodeObjectId`, `calleeTier`, the interpreter `calleeResumePc`, and the
exit's typed `exitReason` and `exitAction`. Capture remains default-off and
bounded; disabled hot calls construct no event.

Scalar query/coercion, static value-load, class-construction, and built-in
Array iterator opcodes share the VM's typed `RuntimeCall` boundary. Their
ABI records the original logical opcode in `template-plan.txt` and
`code-map.json`, but the entry decodes it into a typed descriptor before
semantics begin. The descriptor reads and writes the published `NativeFrame`
window directly, regardless of whether an interpreter `Frame` also exists.
Consequently, an `enteredGenerationDeopt` at one of these opcodes indicates a
real semantic refusal or another unsupported operation in the callee; it is
not an expected representation conversion. For supported scalar/load/class
operations, confirm `jit-generated-call-deopts == 0` and correlate the opcode's
code-map range with `jit-reentrant-stub-transitions`. A high transition count
with zero deopts means semantics stayed stack-owned but the operation itself
remains a hot runtime call.

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
| `optimized-ir.txt` | The optimizing tier's graph: blocks in layout order with every phi and node, its inputs, representation, and eager frame state. |
| `code.bin` | Exact finalized executable bytes for this runtime process. |
| `code-normalized.bin` | Non-executable semantic instruction stream with symbolic relocations and logical branch targets. |
| `asm.txt` | Annotated AArch64 or x86-64 assembly over the exact bytes in `code.bin`. |
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

### Optimizing artifacts

`optimized-ir.txt` starts with `; otter graph`, declares interned constants in
node-id order, then lists the blocks in layout order. Constants use the same
`v<id> = <Kind> [] <Repr>` notation, including exact integer and floating-point
bits. They live outside blocks and are materialized at their uses, so their
declarations do not imply standalone native instruction regions. A block line `b<id> preds=[…]` (with `loop` after the id for a loop
header) is followed by its phis, body nodes, and control node, each as
`v<id> = <Kind> [inputs] <Repr>`. The representation is `None`, `Tagged`,
`Int32`, `Float64`, or `Word`. A node that can deoptimize eagerly appends
`eager=`: one list of `(register, value)` pairs per frame of its inline chain,
outermost first.

`code-map.json` has one `instruction` region per emitted node, in layout order:
`operation` is `v<id> <Kind>`, `operationIndex` the node id, `block` the node's
block, and `functionId` names the source body that owns the node. Its
`logicalPc` / `bytePc` are local to that body, including nested inlined callees;
they do not index the root function's bytecode. One `out-of-line`
structural region covers everything after the body: slow paths, deopt exits,
and the return path.

`deopt.json` lists `frameStates` and `exits`. Each exit names its frame state,
its typed `reason` and `action`, and the `resumePcs` of every frame. Each frame
state lists its frames outermost first with `functionId`, `bytePc`, and slot
recipes (`locationKind`, `locationValue`, `representation`); an inlined
callee's frame adds an `entry` recipe with `returnRegister`, `this`, `closure`,
and `newTarget`.

`safepoints.json` lists every point where generated code may collect:
`taggedLocations` names the native spill slots the collector scans beyond the
frame's register window, which it always scans. A record keeps them as a
bitmap. Template records name none. A Graph record names exactly the tagged
homes written for values live at that boundary, plus the exception scratch;
untagged homes are never named. Collecting slow paths preserve
control-flow-live registers in their canonical homes. Register-only deopt
values materialize into canonical homes on cold exits, and exit recipes read
homes or rematerialized constants. `inlineFrames` names the inlined frames active there; and
`callPc` is the PC of the generated JavaScript call the point belongs to, which
stack walks read as the caller's position.

### Speculation and exits

The optimizing tier builds an SSA graph from bytecode and feedback. Every
instruction becomes specialized nodes, a `Generic` node, or an unconditional
deopt. A `Generic` node is a call that runs the instruction's baseline
(Template) operation on the frame window, so an instruction without a
specialized form never declines the function. An operation that cannot run in
optimized code at all (a suspension, an irreducible loop entry), and a call or
named access that had never run when the snapshot was taken, become
unconditional deopts that resume the interpreter at their PC. An OSR compile
adds a second entry block that reads every register live at the loop header
from the interpreter's window.

A failed check leaves through an eager deopt exit that rebuilds the interpreter
frame before the instruction it guards; `deopt.json` records the exit's
reason. The next compile gives a site that exited a more general form: Float64
arithmetic after an Int32 exit, the shared-table probe instead of inline
property programs after a `shapeGuard` exit.

An exit requesting recompilation retires both native tiers and every installed
caller that actually spliced the failing source function. Ordinary generated
calls continue through the permanent function cell's interpreter destination.
An active invalidated native loop leaves at its next bounded poll, retaining
the old mapping's precise roots and deopt recipes until that exit completes.
The failure history is keyed by its source function, PC and reason, including
the innermost body of an inline exit; the diagnostic outer resume PC remains
the position of the physical compiled activation.

The tier policy then counts only bytecode actually dispatched in the
interpreter. Replacement needs its static work interval plus completion of a
fresh activation or a complete interpreted loop iteration. The first backedge
of a deopt suffix is insufficient; a later visit to the same header proves the
iteration, even with interleaved nested loops. Recursive frames carry distinct
generation evidence. No entry, OSR, generated promotion or new inline body can
bypass that interval, and elapsed compile duration never changes it.

### Property access and calls

A named load or store whose CacheIR feedback programs are all understood runs
them inline (`LoadNamedProperty` / `StoreNamedProperty`): own slots, prototype
holders under a chain proof, and additions in the persistent inline prefix that
publish the child shape and barrier the edge. A receiver no program matches
leaves through an eager deopt. An overflow addition may allocate its suffix;
ordinary stores use `StorePropertyCached` even when that transition is known,
with a resident-suffix hit and a rooted committed miss. Strict overflow stores
retain their baseline operation. One unknown program keeps the whole site
generic. Megamorphic sites, and sites whose earlier generation exited with
`shapeGuard`, lower to
`LoadPropertyCached` / `StorePropertyCached`: the current terminal shared
feedback selects a probe of the isolate's one property action table
(`propertyActionCacheTable` relocation). One receiver shape and atom key
holds independent load and store facts. An inherited holder read and a
different own addition slot can coexist. Native hits prove the live
shape, action, validity and slot-bank capacity before effects; additions
publish the traced child shape and retain both write barriers. Template
uses the same probe after its CacheIR miss. A refused or absent fact
completes the original source operation once through the runtime.
Graph sites without terminal shared feedback retain their existing
CacheIR, committed miss and insufficient-feedback admission policy.

`CallJs` enters a callee proved to be the planned function through its current
generation and any other callee through the generic entry. A call may instead
be inlined under a `CheckFunction` proof of the callee identity: the callee
must be loop-free with no exception handlers, at most 460 bytecode bytes (920
per compilation), at most three levels deep, and the call must lie outside any
exception region of the compiled function. An inlined body's frame states link
to its caller's state after the call, so an exit from it rebuilds the whole
chain of interpreter frames. Monomorphic method calls load the method with
`LoadGuardedMethod`; polymorphic sites switch on the receiver shape
(`LoadReceiverShape`); a megamorphic method is read like a named load and
called through the generic entry. Calls to a declared native leaf keep their
baseline operation as a `Generic` node.

Eligible `LoadGlobalOrThrow` and `LoadGlobalOrUndefined` operations use
`LoadGlobalBinding` when the VM supplies a stable binding proof. Template and
Graph share the same physical guards and live bank reader on each architecture.
The Graph node uses two allocated general-purpose temporaries, remains effectful,
and is neither commoned nor hoisted across calls or stores. Its metadata comes
from the actual source function and byte PC, including inlined bodies.

The hit proves that the active realm is the source realm before reading a live
permanent lexical cell or a guarded global-object slot. Object proofs include
the declarative epoch, current shape or dictionary layout, descriptor flags,
and the current inline or suffix bank capacity. A failed proof or lexical TDZ
hole eagerly exits before the read and reconstructs the complete source-frame
chain; the interpreter performs the canonical operation once. Missing proofs,
writes, existence queries, and shadowed or dynamic bindings retain their
existing baseline operations. Foreign-source proof baking and body inlining
decline; their committed cold operations enter the source realm, while a
disposed source realm fails without falling back to the ambient realm.
`globalLexicalCell` relocations retain the exact source function and byte PC.

Dense tagged and packed-double Array accesses prove ordinary own-slot
semantics through the body's current eligibility byte. A source-realm prototype
override can keep that proof; indexed descriptors, accessors and other sidecar
baggage cannot. Tagged loads also prove the element is present, so a hole
cannot skip prototype lookup. Holey-double access retains the null-sidecar
guard. Every collecting operation invalidates the compiler's cached element
base; later native reads and stores reload it from the rooted receiver.

### Loop-invariant code motion

Per loop, innermost first, the optimizing tier moves checks and loads that
every iteration sees unchanged into a pre-header that every entry into the loop
passes, the OSR entry included. Code moves only out of a loop whose body
neither calls out (including any `Generic` node) nor runs a slow path that may
collect, so no element layout or length changes while the loop runs.
`CheckElements` and `LoadContextParent` always qualify; `CheckShapes` needs a
loop with no named-property store; `LoadElementsLength` needs the same; a
context slot load needs a loop that stores to no slot at its offset. A moved
node reads only values defined before the loop, values moved before it, or a
header phi the loop never changes.

A moved check leaves to the loop header with the values entering the loop, so
the interpreter runs the iteration the check guarded. A header that already
left optimized code for a shape or layout mismatch moves nothing; its next
compile checks where the value is used.

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

### Template call regions

Template call operations record these regions:

| Region kind | Meaning |
| --- | --- |
| `methodGuard` | Method calls only: one region per guarded target, re-reading the receiver shape, prototype validity, holder, and method slot in feedback order. |
| `callTrampoline` | Operand setup, entry through the common call trampoline, and result or throw completion. |
| `tailCall` | A proper tail call (§15.10.3): the callee takes the place of the calling record. |
| `nativeLeafCall` | Identity guard plus direct call of a declared native leaf (see below). |

Every call region carries the caller `functionId`, `logicalPc`, and `bytePc`.
A region at a site with a proven target adds `callTargetFunctionId`; that
target is entered through its permanent function entry cell (a
`functionEntryCell` relocation), so later tier publication switches the
generation it enters without recompiling the caller. A `methodGuard` region
carries a `methodGuard` object with `receiverRegister`, `methodFunctionId`,
`receiverShape`, `prototypeValidity`, `holderRoot`, and `methodField`.
`methodField` and property CacheIR program `field` values contain `bank`
(`inline` or `overflow`), bank-relative `index`, and `byteOffset`. Shape
identity fixes inline capacity; overflow keeps the inline prefix in place. A
guard miss tries the next target; a site that matches no target completes
through the canonical `GetMethod` + `Call` transition without replaying the
caller. A bailout after entry resumes the published callee in the interpreter
and never invokes the call again; its `enteredGenerationDeopt` event names the
exact callee generation and resume PC.

Compiler-generated spread wrappers normally contain the frontend's canonical
`GetIterator` / `IteratorNext` / `ArrayPush` collection loop before the call.
When its source is an ordinary Array whose `@@iterator` is still the original
`Array.prototype.values`, those operations run through the same published
stack-owned activation and the generated wrapper can return without an entry
deopt. An own iterator, prototype replacement, accessor, or non-Array source
misses before observable work and resumes the materialized opcode path. A
generated-call deopt at that guard is therefore a semantic fallback, while
repeated deopts at the default Array collection PCs indicate a regression.
Ordinary-call `inlineCandidate.bakeRejection.kind` distinguishes `polymorphic`
(a retained bounded target population) from `megamorphic` (saturated feedback).
An inline rejection does not by itself describe the final call implementation;
join it with direct-call planning/lowering and compile outcome events.

`CallForwardArguments` stages its complete request (the resolved `apply`,
callee, receiver, and current argument bindings) and then enters the common
call trampoline inside a `callTrampoline` region. The intrinsic `apply`
resolves to the activation's actual arguments, with mapped formals refreshed;
any other method becomes a call with the arguments object. Nothing allocates
between staging and entry.

### Native leaf calls

A Template call whose feedback names a declared native leaf emits a
`nativeLeafCall` region. Its `nativeLeafCall` field names the leaf entry, such
as `math_abs_leaf`, `parse_int_i32_leaf`, or `string_index_of_leaf`, and the
entry also appears as a runtime-stub target in `relocations.json`. The region
guards the exact original builtin identity and calls the entry directly, with
no frame, safepoint, or argument packet: a declared leaf is a pure read, or an
in-place write that misses instead of allocating, and it never reenters
JavaScript. On a plain call, a guard miss deoptimizes the original `Call`
before effects. An explicit-receiver call (`LoadProperty` plus `CallWithThis`)
misses into the ordinary call with the already-loaded callee, which performs
the complete call once. Declarations marked `this_operand` (String methods such
as `charCodeAt` and `indexOf`, Map `get`, Set `has`) read the call's `this` as
their first operand word and prove its type themselves; only `CallWithThis`
lowers them, and plain calls and shaped method sites keep the ordinary call.

Guarded method sites whose method is a declared entry, such as
`"s".indexOf("e")` through `CallMethodValue`, guard the receiver and the method
slot's identity before calling it. A dictionary-mode holder such as
`%String.prototype%` is pinned by its dictionary layout, so adding, deleting,
or redefining one of its properties misses instead of calling a stale builtin.

Both optimizing targets lower pure exact-arity `CallWithThis` declarations
into `CheckNative` followed by `NativeLeaf(stubId)`. The leaf's own `instruction`
region names the actual source function and call PC; its typed `leafValue2`
relocation identifies the VM entry. These calls use canonical tagged input
homes and preserve every live or eager-recovery value before the C call.
The hit has no JS frame, safepoint, source stamp or collecting return site.
A passive miss eagerly resumes that CallWithThis with its loaded callable,
receiver and evaluated arguments; it does not repeat the preceding Get or
argument effects. Plain calls, mutating leaves and guarded method-holder sites
retain their current lowering. When profiling `native-boundary`, separate
crossing counts from work inside each call: the kernel's `indexOf` call keeps
its call boundary.
Contiguous strings return from flattening before constructing a root scope;
rope materialization keeps its traced source and existing write barrier.

These regions and relocation records are captured only when
`--jit-artifacts` is requested. The same generated code runs without building
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
  ; region kind=callTrampoline ... function=<caller> call-target-function=<id> pc=<pc> byte-pc=<byte-pc>
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
  program counter to join the owning code object before applying offsets.
  `entryOffset` names the tier entry over an existing frame; `callEntryOffset`
  names the actual private-JavaScript entry emitted by the frame owner, or is
  explicitly null when no such entry exists. Both use finalized physical code
  bounds. The manifest's `entry` label identifies the compile trigger and does
  not determine whether the resulting whole-function body is callable;
- `relocations.json` describes the symbolic meaning of baked-address ranges;
- `deopt.json` supplies outermost-first frame reconstruction for a `deoptExitId`
  named by the code map or assembly annotation. Each frame includes its function,
  byte PC and register recipes. The outermost `entry` is null; nested entries
  contain `returnRegister`, `this`, `closure` and `newTarget` recipes using the
  same location and representation fields as ordinary slots;
- `safepoints.json` supplies tagged frame/register/spill locations by
  safepoint id and frame state.

`safepoints.json` has one current `{records, returnSites}` document. Each
`returnSites` entry contains the actual `nativeReturnOffset` of a generated
JavaScript CALL/BLR and its `safepointId`; that id selects the source `callPc`,
inline chain and tagged-location inventory in `records`. Several emitted calls
can reference one source record. Proper tail branches create no return site.
The assembly summary lists `js-return +0x<offset> safepoint=<id>`.

The owned generation snapshot independently reports the retained
`call_entry_offset` and `current_entry`, which compares that exact generation's
cell with the permanent function cell's current destination. A linked Template
fallback can remain installed while the function cell selects Graph. An invalid
active generation retains its offset until its mapping retires; the tombstone
then reports null. These snapshots and artifacts are diagnostic evidence, never
callable addresses or an alternative entry-selection owner.

At a collecting boundary, a suspended compiled caller resolves the anchor from
its immediate physical child, including a Host child, or from the canonical
pending call before child publication. Resolution uses the caller's exact
retained code object and mapping range, then an exact table lookup; an address
inside the mapping that is not a registered return is invalid. Active C helpers
retain explicit root/PC publication. No native address is copied into parked
frames, and retired code objects are never selected by scanning history.

Assembly generation is part of explicit artifact capture. Without
`--jit-artifacts`, compilation does not clone finalized code for diagnostics,
run the decoder, format assembly, or perform artifact filesystem I/O.

## Portable code comparisons

`relocations.json` uses `offsetBasis: "code.bin"`. Each sorted record describes the exact
`MOVZ`/`MOVK` range, destination register, emitted chunk shape, and a typed
symbolic target such as a runtime-stub descriptor, GC cage base, property
source cell, string constant cell, or function entry cell. Chunk immediates and
resolved pointer values are deliberately absent. Typed targets use camel-case
fields consistently. A `functionEntryCell` target names only the callee's
function id: the cell selects the callee's current generation at run time.

`code-normalized.bin` starts with the `OTJNCODE` marker, architecture id, and
logical-item count. It is a semantic comparison stream, not ARM64 executable
code:

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

1. Re-run with `--interpreter` to establish the bytecode oracle, then with
   `--jitless` to tell a template-tier defect from an optimizing-tier one.
2. Capture `--jit-events` and find the function's `compilePrepared`,
   call plan/final-lowering events, `compileFinished`, OSR, bail, or deopt
   records. For a static-native site, compare `staticNativeCallPlan` with
   `staticNativeCallLowered` before inspecting artifacts.
3. Join a successful `compileFinished` to `manifest.json` by `codeObjectId`.
4. Read `bytecode.txt` and the first line of the tier input to identify the
   backend and logical operation.
5. Use `code-map.json` to map its logical PC and encoded byte PC to the exact
   native byte range.
   For an optimizing compile, find the region's node in `optimized-ir.txt` by
   its `operationIndex`. For a template call/method inline, first identify the
   caller through `inlineSite`, then inspect its guard, compact scratch
   assignment, callee-local instructions, and separate hit/deopt-teardown
   ranges. For a template call, join caller and callee through the
   `callTrampoline` region's `callTargetFunctionId` and inspect any preceding
   `methodGuard` regions. For a native leaf call, the `nativeLeafCall` region
   names the declared leaf.
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


Canonical bytecode receiver preparation reuses successful field preparation on
its exact finalized constructor family. The family holds the existing
prototype-chain validity cell and reserved field count; there is no separate
root-id proof map. An ordinary constructor still resolves `new.target.prototype`
before selecting the family. A finalized current root, exact collector-updated
selected prototype and valid family-owned proof return before repeated chain
migration, shape registration or watchpoint capture. First publication follows
a bounded no-allocation check of complete ordinary-chain migration and
registration. Partial migration after allocation refusal returns its canonical
reserved prefix without caching and retries on later preparation. Prototype
replacement creates a new family,
finalization clears preparation when it publishes the future root, and real
inherited descriptor/deletion/chain mutations retire the validity cell. Static
field matching runs only when a new actual family is created. Neither tier's
receiver fit guard is relaxed, and property stores retain their source timing.


Generated base-constructor callee entries on both targets can allocate from the
current actual `new.target` family when call-site feedback has saturated. The
`constructor_receiver_probe` LeafValue2 boundary reads the current closure/class
owner, finalized empty capacity root, exact own prototype and the family-owned
nonreviving chain proof. It returns only a boxed integer ticket or a pure miss;
no observable property lookup occurs on the hit. Lazy prototypes, proxy/bound or
unrelated split targets, unfinished/invalid preparation and unavailable LABs
retain canonical preparation once.

The shared LAB writer initializes the complete current object payload and every
reserved inline value before one top publication, then publishes the exact
callee construction ticket/original receiver/this. `constructor_receiver_commit`
uses the existing ContextWords committed pair to update the already admitted
source feedback cells. Missing/malformed completed ownership is Fatal; a
published receiver is never replayed. No safepoint, GC, prototype-proof pointer
or new ABI geometry is added. Fixed caller-plan attempts/miss counters remain
owned by those probes; dynamic prefix fits increment the existing successful
receiver-allocation and physical per-type counters once.
