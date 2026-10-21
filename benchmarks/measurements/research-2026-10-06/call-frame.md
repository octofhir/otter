# Call frame: port V8's StandardFrame + frame-pointer chain

Sources read (benchmarks/measurements/research-2026-10-06/src):
`v8-execution-frame-constants.h`, `v8-execution-arm64-frame-constants-arm64.h`,
`v8-execution-frames.{h,cc}` (StackFrameIterator::Reset/Advance,
ComputeStackFrameType, CommonFrame::ComputeCallerState, EntryFrame::GetCallerState,
ExitFrame::GetStateForFramePointer, MaglevFrame/TypedFrame::Iterate),
`v8-builtins-arm64-builtins-arm64.cc` (Generate_JSEntryVariant, Generate_CEntry,
Generate_CallFunction, InterpreterEntryTrampoline), `v8-codegen-arm64-macro-assembler-arm64.cc`
(Prologue, EnterFrame/LeaveFrame, EnterExitFrame/LeaveExitFrame,
StoreReturnAddressAndCall, InvokePrologue), `v8-codegen-safepoint-table.h`,
`v8-compiler-backend-arm64-code-generator-arm64.cc` (AssembleConstructFrame/AssembleReturn).

## 1. What V8 does

### Layout (StandardFrameConstants, arm64)

```
 [fp + 16 + 8i]  argument i / receiver   caller-pushed, callee reads in place
 [fp + 8]        return address (lr)     kCallerPCOffset
 [fp + 0]        caller fp               kCallerFPOffset
 [fp - 8]        context | type marker   kContextOrFrameTypeOffset
 [fp - 16]       JSFunction              kFunctionOffset
 [fp - 24]       argc (raw)              kArgCOffset
 [fp - 32 ...]   tier body: interpreter/baseline registers, or Maglev/TF spill slots
```

* `Prologue()` = `stp fp, lr; mov fp, sp; stp cp, x1 ; stp x0, padreg` (4 stores).
  Epilogue = `mov sp, fp; ldp fp, lr; drop args; ret`.
* Typed frames (entry, exit, construct, internal, stub) put a Smi marker in the
  context slot (`EnterFrame(type)`), so one 8-byte word at `fp-8` classifies
  every non-JS frame. JS frames are classified by looking up the code
  containing the return pc (`ComputeStackFrameType` → `GetContainingCode`).
* Interpreter and baseline frames are the *same* layout (UnoptimizedFrame):
  baseline ↔ interpreter transfer is an in-place pc change.

### Linkage and publication

* There is **no caller field and no depth**. `CommonFrame::ComputeCallerState`:
  caller fp = `[fp]`, caller pc = `[fp + 8]`, caller sp = fp + 16.
* Calls between JS frames publish nothing. Only transitions into C++ publish:
  `EnterExitFrame` stores `fp` into `IsolateData::c_entry_fp`; fast C calls
  store `fast_c_call_caller_fp/pc`. `StackFrameIterator::Reset` starts from
  `c_entry_fp`.
* Reentry C++ → JS goes through `JSEntry`, which pushes an ENTRY typed frame
  holding the *previous* `c_entry_fp` (and clears it). `EntryFrame::GetCallerState`
  jumps from the entry frame to that saved exit frame, so one fp walk crosses
  every nested segment.
* Stack overflow = stack-limit compare in the prologue (`ldr limit; cmp sp; b.ls`).
  No call-depth counter exists.

### Roots and pcs

* Optimized frames: safepoint table keyed by return pc (`[callee_fp + 8]`)
  gives the tagged-slot bitmap of fp-relative spill slots. Nothing is stamped
  per call.
* Unoptimized frames: every slot is tagged; visited wholesale.
* The current bytecode offset of a baseline frame is recovered from the pc
  (bytecode offset table); the interpreter stores it only around calls.

### Calls

* `x0` argc, `x1` function, `x3` new.target, receiver on the stack with the
  arguments. Root register (`x26`) and cage base are pinned for all JS code;
  nothing passes or saves the isolate per call.
* Receiver conversion and underarity padding happen on the *call* side
  (`Generate_CallFunction`, `InvokePrologue`; optimizing compilers inline both
  for known targets). The callee prologue does neither.
* `[[Construct]]` runs in a separate CONSTRUCT frame built by
  `JSConstructStubGeneric` (receiver allocation via FastNewObject, result
  check). An ordinary call frame carries no construct state.

## 2. What Otter does today (measured)

`crates/otter-vm/src/native_abi/frame.rs` `Frame` = 27 words; generated entry
(`crates/otter-jit/src/arm64/frame.rs::emit_call_entry`) for Graph fib executes
~70 instructions: save x19/x20/x21 + fp, `mov x20, x0`, depth load+carry+limit
(9), stack check (6), caller-return-pc discovery (7–9), pending clear,
depth recomputation (5), RETURN_FRAME store, header words (6), window, this/
new.target/self/argc, construct-request move (5), actuals+caller, depth/
call_site/machine_roots (5), continuation (2), publish `ctx.native_frame`.
Epilogue: RETURN_FRAME load, constructor-owed test, unpublish, restore 3 regs.

earley-boyer hottest code object (f550): 213 of 348 samples are inside call
entry/exit; whole run 80.5 G instr vs node 18.6 G.

Linkage is explicit (`Frame::caller` + `depth` as chain index, innermost
record published in `JitCtx::native_frame` on every call and return), the
return pc is copied into the callee record (`caller_return_pc`, plus the
`CallRequest.caller/caller_return_pc` mailbox for trampoline-classified calls).

## 3. Target (Otter port)

### 3.1 One frame shape, fp-anchored

Every activation record (interpreter, host, baseline, optimizing) sits at a
fixed distance below the machine frame pointer of the machine frame that
hosts it: `record = fp - FRAME_RECORD_OFFSET`, and the record's last word is
the descriptor at `fp - 8` (V8's context-or-marker slot). `[fp]` / `[fp+8]`
are the caller fp and the return address. Trampoline extents that host no
record write only the descriptor word with kind `Bridge`; the outermost
trampoline extent of a Rust entry writes kind `Entry` plus the enclosing
segment's published fp (V8 ENTRY frame).

* Trampoline-built interpreter/host records: the record is placed directly
  below the trampoline's fp (callee-saved spills move below it), so the record
  belongs to the trampoline's machine frame; a tier entry over that record
  runs on the same fp (V8 baseline/interpreter in-place transfer).
* Generated entries build their record directly below their own fp.

### 3.2 Linkage = fp chain

* Remove `Frame::caller`, `Frame::depth`, `Frame::caller_return_pc`,
  RETURN_FRAME, `CallRequest::caller/caller_return_pc` and every per-call
  publication of `JitCtx::native_frame`.
* `Frame::caller_frame()` = walk `[fp]` skipping `Bridge`, crossing `Entry`
  through its saved fp.
* Return pc into a record = `[inner_fp + 8]` of the machine frame it called
  (V8 `state->pc_address`).
* `JitCtx::native_frame` becomes V8's `c_entry_fp`: written only on
  generated→Rust transitions (runtime stub calls, trampoline → interpreter /
  host entries). Generated JS→JS calls never touch it.

### 3.3 Depth

* No logical depth counter. Overflow = native stack limit (already checked
  in every prologue and in the trampoline reservation).
* `ActivationFloor` / chain indices become frame addresses (the stack grows
  down: a frame is above a floor iff its fp is below the floor fp).

### 3.4 Record contents (V8 fixed slots)

`descriptor (kind|flags|register_count|code_object_id) @ fp-8`,
`function (SELF) @ fp-16`, `argc @ fp-24`, receiver, new.target, then the
tier body. Construct-only fields move to the construct frame (subsystem 3).
`call_site`/`machine_roots`/`header.pc` stamps are replaced by return-pc
lookup (exit publishes fp and pc, V8 fast-C-call fp/pc pair); spill homes are
fp-relative constants of the code object.

### 3.5 Registers

`x20`/`r15` = JitCtx pinned for all generated code of one entry (V8 root
register): never passed in `x0`, never saved per JS call. The record register
(`x21`) disappears (record = fp − constant). Template keeps `x19` as window
base, saved only by Template bodies.

## 4. Port order

1. Linkage + layout (3.1, 3.2, 3.3): fp-anchored records, Bridge/Entry
   markers, exit-only publication, no caller/depth/caller_return_pc, floors as
   addresses. Both architectures, trampoline + Template + Graph + Rust walkers.
2. Contents + pinned ctx (3.4 fixed slots, 3.5): descriptor/function/argc slots,
   ctx pinned, x21 gone, receiver conversion and underarity at call sites.
3. Exit pc publication: stub calls publish (fp, pc); call_site / header.pc
   stamps and machine_roots removed; safepoints and source pcs by return pc.
4. Construct frame (with subsystem 3 `new`).
