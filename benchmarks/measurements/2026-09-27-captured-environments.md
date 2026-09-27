# Captured environments: rejected representation

Status: rejected after all seven fixed-work measurements. The production binding
representation has been restored; no alternative mode remains.
Pre-change signed revision: `cd3aa798d0c7e0c2bc0e4ca9c9ca8a59768899c3` (`G`).
Preserved production executable: `benchmarks/results/parity-2026-09-27/otter-before-environments`.
The indexed-access report contains the complete seven-workload instruction/RSS
baseline, code size, compiler timing and passing mandatory gates.

## Evidence

After the two completed slices, Earley–Boyer still retires 330,993,009,756
instructions: 17.76× Node/V8 and 15.09× Bun/JSC. Peak RSS is 135,331,840 bytes.
An uninstrumented successful run has 11,946 isolate self samples:

- Generated code: 1,975 (16.53%).
- OldSpace::alloc: 405 (3.39%), down from 17.26% before the free-list replacement.
- Upvalue allocation: 349; capture-spine allocation: 304; generated-upvalue
  initialization: 249. Their disjoint self samples sum to 902 (7.55%).
- Generated-upvalue initialization: 894 inclusive, with 500 directly below it
  in cell allocation. Closure allocation: 731 inclusive, including 431 in spine
  allocation. These inclusive rows overlap and must not be added.
- Scavenging, marking and sweeping remain substantial. This profile does not
  establish how much of their cost was caused by captures.

Raw data: `plain-earley-element/`, `earley-element-attribution.txt`, and
`earley-element-children.txt` under the parity result directory. The new
`otter-allocation-probe script <path>` command reads existing per-tag counters
outside execution to quantify allocation traffic without adding hot-path counters.

## Primary implementations

- [V8 Context](https://github.com/v8/v8/blob/main/src/objects/contexts.h) stores
  scope metadata, a previous-context link and a trailing tagged slot array.
  Slot offsets follow the physical element index; they do not require a separate
  GC object for every ordinary captured binding. Specialized ContextCell objects
  also exist, so this is not a claim that V8 eliminates every indirect cell.
- [JSC JSLexicalEnvironment](https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/runtime/JSLexicalEnvironment.h)
  places write-barrier value slots after the environment header. ScopeOffset
  selects a variable, and allocation size is header plus scope-size times slot
  size. The symbol table owns the correspondence between names and slots.
- [SpiderMonkey EnvironmentObject](https://github.com/mozilla-firefox/firefox/blob/main/js/src/vm/EnvironmentObject.h)
  distinguishes function and block environments, uses environment coordinates
  to select fixed/dynamic binding slots, and omits heap environments when all
  bindings can stay on the stack. CallObject owns its callee plus an enclosing
  environment link. Otter must preserve per-binding renewal and sharing when
  adapting grouped storage to its existing flat capture coordinates.

Downloaded source snapshots are in `sources/`. Otter must preserve its flat
compiler capture mapping, individual loop-binding renewal, direct-eval adoption,
shared arguments/derived-this bindings and precise moving roots. A context-chain
analogy alone does not establish those semantics.

## Experimental contract

Replace one old-space object per binding with a variable-sized captured
environment and a typed `(environment handle, slot index)` binding reference.
Initialize all ordinary own captures in one allocation. Inherited captures,
loop renewal and dynamic bindings use the same reference representation;
there is no old-cell adapter or second binding mechanism.

The experiment implemented these vertical changes:

1. Environment owns trailing Values. Every reference traces its actual owner
   field in place; the slot index is ordinary immutable data. Do not cast a
   composite reference to RawGc at arbitrary call sites.
2. Interpreter, generated callee initialization and resumed frames construct the
   same references. Native frame/spine stride, both JIT targets, binding proofs
   and write barriers use one VM-owned layout contract.
3. A loop renewal allocates a fresh slot and replaces that binding reference,
   preserving other aliases. Grouping a whole frame must not accidentally copy
   shared mutable bindings during a loop iteration.
4. Ordinary environments may move. Persistent global lexical proofs require
   stable old owners. No raw environment/value address may survive collection,
   reentry or a generated backedge.
5. Snapshot/restore retains owner and index together. Heap image tracing,
   code liveness, eval, mapped arguments and derived-this users must all change.
6. Variable young allocation must honor `trace_pending_slots`, not walk a
   stack-resident body's nonexistent trailing storage. Audit the existing
   `alloc_trailing_with_roots` call before admitting a traced environment tail.
7. Grouping can retain otherwise-dead sibling values; larger references can grow
   spines and frames. Measure these memory costs explicitly. Existing TS peak
   RSS increased about 1.3% in the indexed-access slice and remains unresolved.

## Allocation census before replacement

The probe runs the original script through Runtime production tiering and reads
existing counters before/after execution. It is diagnostic attribution, not a
substitute for CLI retired-instruction/RSS counters. Both scripts completed as
`undefined` without errors. Reported live bytes are not a forced-GC retained set.

| Workload | Total allocation bytes | Upvalue cells / bytes | Spines / bytes | Closures / bytes | Minor / full GC |
|---|---:|---:|---:|---:|---:|
| earley | 17,742,263,792 | 110,900,416 / 1,774,406,656 | 10,172,901 / 559,846,416 | 10,193,283 / 1,141,647,696 | 987 / 606 |
| ts | 1,028,544,200 | 1,725,721 / 27,611,536 | 394,391 / 9,362,400 | 395,271 / 44,270,352 | 39 / 5 |

This accepts the grouped-environment candidate for implementation. The main
opportunity is the number of distinct old-space binding objects and repeated
allocation/root publication, not merely the 10% byte share of cells in Earley.
The old pinned spine is also a generational constraint: young environments
reachable from old spines can survive minor GC until their dead old owners are
swept. Measure that effect before selecting the final allocation policy.


## Validation of the rejected experiment

The experimental patch replaced individual binding bodies with one trailing-Value
environment per invocation and environment/index references. Both JIT targets
use an eight-byte native spine stride. The slot index survives owner relocation;
stores barrier the environment. Heap closure derived-this uses the null owner
for absence so the new reference does not add an Option discriminant to every
closure. Snapshots carry the global reference index explicitly.

Initial focused validation: 13 upvalue tests, 4 pending-tail GC tests and
283 JIT tests passed. Interpreter, Template and production execution of the new
alias corpus matched Node output without stress. Stress stride 1 exposed copied
mapped-arguments references that were not rooted before shape/object allocation.
The preserved pre-environment executable passes that same case: old pinned
binding cells masked this omission. The experiment roots those copies
from collection through initialization, roots installation across sidecar
reservation, and retains mapped store references/RHS across descriptor
allocation. The rebuilt CLI passes this corpus with stride 1 and slot
verification in interpreter, Template and production modes, exactly matching
Node stdout and with empty stderr. The complete differential gate passes
58/58 cases; the new alias case additionally passes every stride 1 through 16.
Final focused VM tests pass: upvalue 13, closure allocation roots 2, snapshot
with nonzero environment index 1, mapped-argument behavior 2. The runtime
counter-closure reclamation regression passes and returns environment live
bytes to its pre-script baseline after a full GC.

Final JIT gate: 283 passed. Cross-target `otter-jit --tests` check for
`x86_64-apple-darwin` passed; this ARM64 host did not execute x86 code.
VM and runtime compile-fail boundaries pass after updating the four affected
diagnostic fixtures, then pass again without overwrite. Environment references,
Frame and NativeCtx still fail Send bounds. Raw validation logs use the
`environment-` prefix in the parity result directory.

## Complete fixed-work result and rejection

Adjacent before/after runs use `/usr/bin/time -l` and identical workload hashes.
Every run exits 0 and before/after stdout agrees. Raw commands, executable hashes
and counters are in `environment-reference/` and `environment-final/`.
RSS is bytes; the original crypto remains unseeded as agreed.

| Workload | Before instructions | Experiment instructions | Change | Before RSS | Experiment RSS | Change |
|---|---:|---:|---:|---:|---:|---:|
| ts | 203,134,806,038 | 205,041,253,825 | +0.94% | 671,662,080 | 680,509,440 | +1.32% |
| zlib | 274,851,779,254 | 279,227,298,450 | +1.59% | 784,678,912 | 828,522,496 | +5.59% |
| crypto | 16,978,964,572 | 17,059,503,997 | +0.47% | 50,298,880 | 50,626,560 | +0.65% |
| fib | 3,843,772,981 | 3,884,756,967 | +1.07% | 58,769,408 | 58,982,400 | +0.36% |
| mega_method | 10,152,207,238 | 10,210,181,641 | +0.57% | 59,392,000 | 59,441,152 | +0.08% |
| ast_ctor | 32,649,884,389 | 32,662,779,405 | +0.04% | 129,581,056 | 129,564,672 | -0.01% |
| earley-boyer | 330,818,060,921 | 323,314,666,899 | -2.27% | 135,086,080 | 155,123,712 | +14.83% |

The small Earley instruction gain does not justify its 14.83% RSS regression
and the regressions elsewhere. This owner/index representation is rejected.
The source patch and binaries are preserved as `rejected-captured-environments.patch`,
`otter-rejected-environments` and `allocation-probe-rejected-environments` in the
ignored parity artifact directory. All experimental VM/JIT consumers, docs and
fixtures were removed together. No compatibility path or feature flag remains.
The new alias corpus remains as a semantic regression case. The independent
pending-tail tracing correction and its forced-GC test remain: stack pending
payloads must invoke the pending tracer, never the allocated-tail tracer.

## Allocation attribution

| Workload | Metric | Before | Experiment |
|---|---|---:|---:|
| Earley | Environment allocations | 110,900,416 | 20,345,413 |
| Earley | Environment bytes | 1,774,406,656 | 1,049,966,632 |
| Earley | Capture-spine bytes | 559,846,416 | 1,037,629,624 |
| Earley | Total allocation bytes | 17,742,263,792 | 17,495,606,976 |
| Earley | Minor / full GC | 987 / 606 | 904 / 523 |
| TS | Environment allocations | 1,725,721 | 792,469 |
| TS | Environment bytes | 27,611,536 | 20,145,520 |
| TS | Capture-spine bytes | 9,362,400 | 13,907,376 |
| TS | Total allocation bytes | 1,028,544,200 | 1,025,619,296 |
| TS | Minor / full GC | 39 / 5 | 39 / 5 |

Earley still stores exactly 110,900,416 logical bindings in the experiment:
`environmentBytes / 8 - environmentAllocations`. Its 724,440,024-byte environment
saving is largely consumed by 477,783,208 additional spine bytes. Group retention
and remembered old spines also change GC lifetimes; the counters alone do not
quantify their separate shares of peak RSS. Reported end-of-script live bytes
are not comparable retained sets without an explicit common collection point.

Zlib event-only capture completes without truncation: 57 compile completions,
3,857,484,919 ns compile time, 15,332,236 generated bytes. Previous indexed-final
capture had 57 completions, 3,872,612,000 ns and 15,257,860 bytes. Code size grew
0.49%; timing is a single diagnostic observation. These are separate executions
from the resource-counter runs.

The result rejects grouping underneath flat per-binding capture spines, not all
lexical-environment designs. Revisiting environments requires changing compiler
capture coordinates and closure ownership together, with a measured benefit
that covers the memory cost. The next investigation targets arguments object
materialization: the existing Earley census attributes 5,022,525,056 allocated
bytes to exotic and symbol sidecars, almost entirely alongside 14.6 million
arguments constructions. Allocation elimination or dedicated storage needs a
separate semantic and profile audit before implementation.

After removing the experiment, the retained state passes differential 58/58,
JIT 283/283 and pending-tail GC 4/4. Logs: `environment-rejection-*`.
