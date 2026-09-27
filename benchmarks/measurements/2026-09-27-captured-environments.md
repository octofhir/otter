# Captured environments: measured next boundary

Status: investigation and allocation census; no implementation result claimed.
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

## Candidate contract to validate

Replace one old-space object per binding with a variable-sized captured
environment and a typed `(environment handle, slot index)` binding reference.
Initialize all ordinary own captures in one allocation. Inherited captures,
loop renewal and dynamic bindings use the same reference representation;
there is no old-cell adapter or second binding mechanism.

Required vertical changes before this is a usable implementation:

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

The census below is complete. Next executable step: replace the binding-cell
representation and batch own-binding initialization, then update every tracing,
JIT layout, barrier and snapshot consumer. Exercise binding alias/loop/eval/
snapshot and moving-GC regressions, run both mandatory gates, and repeat every
agreed fixed-work measurement.

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
