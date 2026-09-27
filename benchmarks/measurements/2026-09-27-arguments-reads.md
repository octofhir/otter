# Activation-local arguments reads

Base: `866b7be5c8148583c79046b0c449cb728584f9a2`, macOS ARM64.
Implementation and final resource measurements are complete. No parity claim.

## Measured mechanism

A fresh native profile of the retained production engine has 2353 isolate self
samples. Joining caller PCs to emitted code attributes 807 inclusive samples
inside `collect_arguments_value` to `sc_list` (function 594), 195 to `sc_append`
(604), and one to the benchmark driver. These are stack samples, not retired
instruction counts, and must not be added to self samples. Raw artifacts:
`benchmarks/results/parity-2026-09-27/earley-arguments-profile/`.

The earlier allocation census counts 14.6 million arguments-related exotic and
symbol sidecars totaling 5,022,525,056 bytes. `sc_list` copies `arguments` into a
local, then reads length and indexed values while constructing pairs.

## Primary implementations

[JSC arguments elimination](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/JavaScriptCore/dfg/DFGArgumentsEliminationPhase.cpp)
tracks argument allocations and consumers through SSA, rejects escapes and
interference, and accounts for prototype behavior on out-of-bounds reads.
[SpiderMonkey scalar replacement](https://raw.githubusercontent.com/mozilla-firefox/firefox/main/js/src/jit/ScalarReplacement.cpp)
checks arguments uses, mapped parameters and recoverable state before replacing
object operations with frame reads.
[V8 create lowering](https://raw.githubusercontent.com/v8/v8/main/src/compiler/js-create-lowering.cc)
separates mapped/unmapped arguments and their backing storage. Saved source
hashes are in `arguments-sources.json` under the raw result directory.

## Replacement

The compiler proves every use of the implicit arguments identity after AST
lowering, following register/local copies through branches and loops. Only
zero-formal, non-rest, non-eval, non-suspending functions are currently admitted.
An escaping use, a mixed receiver at a join, or unsupported control flow keeps
the canonical object. The bytecode verifier enforces the activation metadata.

Admitted `.length` and indexed reads become explicit activation operations.
The interpreter, Template and Machine tiers read the incoming actual window.
Machine represents its probe, cold call, exception/status control and result
join before register allocation. ARM64 and x86-64 share their target probe
between Template and Machine. No source-name or benchmark-specific selection.

A non-Int32 key or out-of-range index first creates the canonical arguments
object, then performs ordinary property access. A getter can observe and mutate
that object; all later operations use the same traced identity. The native cache
uses the previous four bytes of tail padding: NativeFrame remains 72 bytes.
Entry, GC tracing, exact deopt and return copy the current identity. No raw
window address survives reentry or a backedge. Keys are rooted before allocation.

Arguments construction now retains the realm's original Array values intrinsic
instead of reading mutable `Array.prototype.values`. This is necessary for
correct delayed materialization. The intrinsic participates in both GC roots
and the fixed snapshot root walk.

## Validation and measurements

The first complete candidate passes differential 59/59 and JIT 283/283,
bytecode 96/96, compiler analysis 4/4, native root rewrite 1/1, intrinsic roots
6/6, generated-arguments linkage 2/2, local Machine/cold identity and restored
iterator tests 2/2, and the snapshot round-trip test. x86-64 cross-compilation
passes; execution on that architecture was not performed. GC stress 1..16
matches Node in all three tiers (48 runs), with slot verification enabled.
A separate `with` diagnostic covers implicit lookup, aliases and shadowing in
all tiers at stress 1. Its output is `3:2 7:2 41:9`, matching Node.

The Machine artifact test asserts the probe in the exact `sumArguments` and
`coldArguments` bodies. A larger cold fixture with an allocation loop exposed
an existing optimizing rejection, `numeric checked operation deopt label`,
at OSR; it retains Template execution. The committed differential corpus keeps
allocation-loop coverage, while the separate Machine test exercises normal-entry
cold materialization and getter mutation. This OSR limitation remains open.

The baseline executable is the preserved indexed-access production build
`otter-before-environments` (from `cd3aa798`); the candidate also includes the
independent pending-tail tracing fix committed in `866b7be5`. Commands and
executable/script hashes are retained with every counter run. The discarded
grouped-environment implementation is absent from both executables.

### Allocation attribution

The diagnostic script census compares the preserved baseline with the first
arguments candidate. Its counts are independent of CLI resource measurements.

| Earley metric | Before | After |
|---|---:|---:|
| Allocated bytes | 17,742,263,792 | 10,974,003,256 |
| Ordinary object allocations | 87,950,785 | 73,350,583 |
| Exotic sidecar allocations | 14,600,504 | 302 |
| Symbol sidecar allocations | 14,600,202 | 0 |
| Minor collections | 987 | 638 |
| Full collections | 606 | 257 |

This removes 14,600,202 arguments objects and their two sidecars. Allocation
traffic falls 38.15%. Reported live bytes after the script remain approximately
88 MB; this is not a forced-collection retained-set measurement. TS allocation
traffic is essentially unchanged: 1,028,544,200 → 1,028,544,312 bytes, with the
same 39 minor / 5 full collections.

### First full resource pass

`arguments-reference/` and `arguments-final/` contain all seven original workloads,
all exit 0 with identical stdout. Earley instructions: 331,008,196,683 →
239,888,077,581 (−27.53%); RSS: 135,708,672 → 133,939,200 bytes (−1.30%).
This candidate added a separate cache-zero store to each generated frame;
fib instructions rose 0.86%. The final implementation clears the cache using
the existing adjacent argument-count store, on both backends.

TS in that first pair is +0.42% instructions / +0.81% RSS. A repeat is +0.13%
instructions / −3.89% RSS (694,419,456 → 667,435,008 bytes), demonstrating that a
sub-percent RSS change from one pair is not reliable evidence of a memory win.
Zlib is +0.007% instructions / −3.26% RSS in the first pair. Final counters follow
below; the initial candidate is not used to claim parity.

### Final resource pass

The final count/cache initialization uses one store. All seven runs exit 0 and
match baseline stdout. `arguments-count-final/` retains exact commands, counters
and hashes; `arguments-reference/` is the preserved-binary baseline.

| Workload | Before instructions | Final instructions | Change | Before RSS bytes | Final RSS bytes | Otter / V8 | Otter / JSC |
|---|---:|---:|---:|---:|---:|---:|---:|
| ts | 203,594,718,663 | 203,814,859,650 | +0.11% | 691,060,736 | 705,085,440 | 9.70× | 9.83× |
| zlib | 274,882,757,013 | 275,020,013,293 | +0.05% | 811,679,744 | 829,276,160 | 11.89× | 11.81× |
| crypto | 17,069,609,792 | 17,038,085,240 | -0.18% | 50,282,496 | 50,839,552 | 5.81× | 7.65× |
| fib | 3,846,216,978 | 3,843,625,830 | -0.07% | 59,113,472 | 59,604,992 | 4.03× | 7.13× |
| mega_method | 10,153,940,287 | 10,156,102,063 | +0.02% | 59,424,768 | 59,277,312 | 2.51× | 6.21× |
| ast_ctor | 32,663,798,352 | 32,652,201,837 | -0.04% | 129,843,200 | 129,957,888 | 9.91× | 15.85× |
| earley-boyer | 331,008,196,683 | 239,692,899,092 | -27.59% | 135,708,672 | 135,086,080 | 12.86× | 10.93× |

Fib returns to baseline instruction cost (−0.067%). Earley removes 91.3 billion
instructions (−27.59%). Its final RSS is only 0.46% lower. TS/zlib RSS in this
pass is respectively 2.03% / 2.17% above the reference; earlier candidate runs
varied in both directions despite unchanged TS allocation counts and nearly
unchanged zlib code size. Therefore this slice establishes a large reduction in
allocation traffic, not a robust peak-RSS improvement across the corpus.

V8/JSC denominators are the original seven-workload measurements documented in
the indexed-access report, using Node 24.16.0 and Bun 1.4.3-canary.1. No parity
has been reached.

### Compilation and remaining hot work

Event-only captures are complete and untruncated. Earley: 160 attempts / 156
successful compiles before, 159 / 159 after; summed compiler time 92,052,587 →
86,229,589 ns; generated code 1,641,144 → 1,656,084 bytes (+0.91%). Both measured
arguments consumers now have Machine compilations. Zlib retains 57 attempts / 54
successes and 15,257,860 generated bytes; compiler time is approximately 3.88 s.
Compiler duration is an observation from one diagnostic run, not a stable speedup.

The successful final Earley profile contains 1739 isolate self samples. Upvalue
allocation contributes 119; spine allocation 82; generated-upvalue initialization
60; runtime closure construction 68 + 30; closure allocation 20. These disjoint
self samples sum to 379 (21.8%). GC, old allocation and root walking add work,
but their complete cost cannot be attributed to captures from this profile alone.
The census still allocates 110,900,416 binding cells. This is the next measured
mechanism to investigate. The rejected owner/index environment representation
must not be revived without addressing its larger references and RSS regression.

Final validation records are `arguments-count-{jit-gate.txt,difftest-gate.json,
stress.json,x86-check.txt}` under the raw result root. The final stress record
contains all 48 successful runs, source/executable hashes and Node comparisons.
The final native profile is `earley-after-arguments-profile/`.
