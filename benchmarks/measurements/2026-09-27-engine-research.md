# Fixed-work parity investigation

Starting revision: `1944cbee14c3ddff7c7bc815682b7a09d27db461`, macOS ARM64.
Primary sources inspected on 2026-09-27. Downloaded source snapshots are in
`benchmarks/results/parity-2026-09-27/sources/`. Links to moving upstream
branches identify the source; local snapshots preserve what was read.

## Mechanisms and constraints

### V8

[Maglev graph builder](https://raw.githubusercontent.com/v8/v8/main/src/maglev/maglev-graph-builder.cc),
`TryReduceFunctionPrototypeCall`: takes the receiver as the target, removes
the outer receiver and feeds the shifted arguments to `ReduceCall`. This
reuses normal call optimization after proving the builtin target. The hot
path need not enter a native forwarding function. A failed identity or target
proof must precede callee effects; call state must still describe the original
source operation for deoptimization. GC requirements are those of the normal
generated call, including the live callable and receiver.

[Map layout](https://chromium.googlesource.com/v8/v8/+/main/src/objects/map.h)
contains prototype and prototype validity cell fields. This gives a structural
place to tie property proofs to prototype identity. Otter cannot adopt the
guard simplification alone: allocation, prototype mutations, dictionary
transitions, traced ownership and every cache key must change together.

### JavaScriptCore

[DFG parser](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/JavaScriptCore/dfg/DFGByteCodeParser.cpp),
`BoundFunctionCallIntrinsic`: checks the exact bound function, builds target
operands, and returns to normal call handling. Frame locals remain recoverable
at exits before the target has run. This is applicable to removing forwarding
wrappers in Otter while preserving observable target invocation exactly once.

[DFG effects](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/JavaScriptCore/dfg/DFGClobberize.h)
models stack reads for inlined frames and broad effects for calls and coercion.
The cost belongs in compiler metadata; generated guards can be moved only
when effects permit it. Otter's moving GC additionally requires rewritten
roots after runtime reentry; a cached raw object address is insufficient.

### SpiderMonkey

[Baseline CacheIR compiler](https://raw.githubusercontent.com/mozilla-firefox/firefox/main/js/src/jit/BaselineCacheIRCompiler.cpp),
`updateArgc`, `prepareForArguments`, `pushFunCallArguments`: `FunCall` subtracts
one actual argument when present, maps target/receiver/arguments onto the
ordinary call stack, and supplies undefined for the zero-argument receiver.
The path shares stack alignment, missing-parameter filling and callee-token
publication. Otter can similarly reuse generated linkage after builtin and
target guards. Proxy, bound and unsupported targets still require canonical
call semantics; a cache miss must not replay a started call.

### HotSpot

[AArch64 shared runtime](https://raw.githubusercontent.com/openjdk/jdk/master/src/hotspot/cpu/aarch64/sharedRuntime_aarch64.cpp),
`RegisterSaver::save_live_registers` and safepoint handler generation: saved
register locations enter OopMaps keyed to runtime call positions. Runtime
entry publishes a valid last Java frame; GC and deoptimization share precise
location metadata. This suggests reducing repeated Otter frame publication
only after a complete stack-walk/root contract exists. Java's fixed signatures
and object semantics do not justify removing JS arity, this, Proxy or exception
handling. Miss cost includes register saving and frame reconstruction.

[Profile caching research](https://cr.openjdk.org/~thartmann/papers/2017-ManLang-Profile_Caching.pdf)
is background on tiered profile reuse, not evidence that any particular Otter
cache size or compilation threshold is profitable.

### BEAM

[AArch64 calls](https://raw.githubusercontent.com/erlang/otp/master/erts/emulator/beam/jit/arm/instr_call.cpp),
`emit_i_call`, `emit_i_call_only`, `emit_dispatch_return`: local calls branch
to generated code; tail calls release the Erlang frame; the return sequence
checks reductions and branches to shared scheduling code on exhaustion.
[BeamAsm runtime documentation](https://www.erlang.org/doc/apps/erts/beamasm.html)
describes explicit runtime transitions and valid stack terms at collection.
This supports cold shared transitions and cheap generated linkage in Otter,
but Erlang reductions and its stack/term model are not JS call semantics.
GC and exception inspection must never see a partially published frame.

## Measurement contract

`scripts/dev/fixed-work.py` runs the seven requested scripts sequentially,
records `/usr/bin/time -l` counters, exit status, exact commands, executable
and workload SHA-256, and host load. Node measures V8; Bun measures its embedded
JSC with Bun host overhead, not a standalone JSC shell. Comparisons include
startup and compilation. Artifact/sample runs are separate from counter runs.
Sampling identifies likely instruction costs; it is not an instruction counter.
No parity claim is justified before the complete corpus succeeds.

## Rust allocator API follow-up

The user asked whether Rust 1.100 allocator support justifies nightly.
[Stabilization PR #156882](https://github.com/rust-lang/rust/pull/156882) merged
on 2026-09-23 (`f4b7c670f92a6d5459f6929ae09e13cc05323566`), ahead of the
1.100 branch date discussed in that PR. Upstream `Vec::new_in` is marked stable;
`try_with_capacity_in` and const use remain under `allocator_ext`.
The workspace currently builds with Rust 1.97.1 / LLVM 22.1.6. Its previously
installed nightly was 1.98.0-nightly from 2026-06-29. Separately installed
`nightly-2026-09-27` reports Rust 1.101.0-nightly (`75a75c3e0`, 2026-09-26).
A standalone probe compiled and ran `Allocator`, `Vec::new_in` and
`Box::new_in` without feature attributes on that toolchain. The workspace
default was not changed. Probe source and executable are retained under
`benchmarks/results/parity-2026-09-27/allocator-api-probe*`.

A concrete candidate is an arena scoped to one JIT compilation for transient
IR/CFG/analysis containers. The allocator must preserve nonmoving storage and
destruct owned values correctly before arena release. Generated code, safepoint
tables, code-owned roots and persistent artifacts cannot borrow that arena.
The [Allocator contract](https://github.com/rust-lang/rust/blob/main/library/core/src/alloc/mod.rs)
requires live allocations to retain valid pointers; a moving JS heap cannot
be substituted for a standard collection's allocator without violating it.

Decision: finish the measured old-space mechanism change on the original
compiler. Evaluate a pinned newer toolchain and compiler arena separately so
LLVM/toolchain differences cannot masquerade as an allocator improvement.
