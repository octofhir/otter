# Generated-code call and frame contract: current state (2026-09-29)

Read-only map taken at `f6acbbbc`, input for the call/frame redesign
(architecture report, "Slices after E1"). Measured cost: the Machine `fib`
recursive call is ~150 instructions from argument setup to post-return
(V8 ~20).

## Register conventions
- Template arm64: x19 register-window base, x20 `JitCtx`, x21 current
  `NativeFrame` (`template/arm64.rs:2011-2022`). Template x86: r15 ctx, r14
  NativeFrame, r13 register base.
- Machine arm64: x19 `JitCtx`, x17 register base loaded in the prologue
  (`machine/numeric/arm64.rs:5031-5068`). Machine x86: r15 ctx.
- Shared arm64 linkage `arm64/direct_call.rs::emit_direct_call_with_access`
  serves both tiers through `context_register` (Template 20, Machine 19).

## NativeFrame (56 B, `native_abi/frame.rs`)
| Offset | Field | Eager writers | Readers |
|---|---|---|---|
| 0x00 u32 | function_id | entry, linkage (copied from `CodeEntryCell.native_frame_header`), inline parents | tier-up mailbox, `RuntimeCall::bind`, deopt, `generated_caller_matches`, stack snapshots |
| 0x04 u32 | pc | Template stamps before every exiting/calling op; Machine only before committed/generic/forward calls (not before arm64 direct calls) | exit payloads, deopt pc checks, every `RuntimeCall` operand decode, stack traces |
| 0x08 u16 | register_count | entry, linkage (parameter-prefix count), cold-exit widening, deopt | window validation, arguments fast path, deopt, alloc-safepoint bounds |
| 0x0A u8 | kind | entry, linkage, inline parents | only `generated.rs:153` |
| 0x0B u8 | flags | HAS_SAFEPOINTS (no reader), STACK_REGISTERS (GC, bind, deopt, snapshot), DERIVED_CONSTRUCTOR, INCOMING_ARGUMENTS, OSR_ENTRY | |
| 0x10 | register_base | entry / linkage `sp + layout.register_base` | Template prologue, Machine EntryValue/OSR/deopt, GC |
| window | registers | linkage copies args, fills locals with `undefined` | GC traces the whole window for the whole call even for Machine callees (values live in SSA; window written only on deopt) |
| 0x18 | this | linkage | LoadThis, construct result, GC, deopt |
| 0x20 | new_target | linkage | super-construct, LoadNewTarget, GC, deopt |
| 0x28 | SELF | linkage | LoadClosureContext/LoadSelf, GC, deopt |
| 0x30 u32 | argument_count | linkage (64-bit store also zeroes arguments_object) | arguments fast path, deopt |
| 0x34 u32 | arguments_object | zeroed; materializers | arguments, GC, deopt |

## Thread state
- `JitCtx` (120 B, per outer entry on the Rust stack): 0x00 thread, 0x08
  native_frame (swapped per call), 0x10 error slot, 0x18 activation array
  base, 0x20 activation top ptr, 0x28 activation limit, 0x30 global_this
  offset ptr, 0x38 native stack limit, 0x40 generated_feedback_clean (zeroed
  before every `blr`), 0x48 machine_roots head, 0x50.. allocation window,
  runtime_stats.
- `VmThread` (96 B): current_frame, current_code_object_id, runtime_context,
  code_registry, interrupt/heap/fuel/epoch/marking/protector/realm cells.
- Frame list is a flat array `Interpreter.jit_native_activations` with a top
  cursor; every generated call pushes/pops it.

## GC roots in Machine frames
- `JitMachineRootRecord` (32 B: previous, root_base, code_object_id,
  root_count, safepoint_id) pushed/popped around every Machine call; roots
  copied to root-save homes before, reloaded after.
- `trace_native_jit_activations` traces each published frame's window plus
  this/self/new_target/arguments_object, then every root record's homes
  (safepoint id not consulted). Allocating stubs get
  `RuntimeStubAllocContext{thread, spill_slots, safepoint_id, count}`.
- Safepoints are keyed by `(code_object_id, safepoint_id)` only; the
  return-offset tables (`SafepointEntry.native_return_offset`, `deopt.rs`
  `StackMap`) are vestigial. Machine frames are not fp-chained (x29 saved,
  never set).

## Return protocol (`NativeResultPair`, x0 payload / x1 status)
0 Success, 1 SideExit (`pc | reason<<32 | action<<40`), 2 Throw, 3 Continue,
4 OutOfMemory (probe), 5 Yield (poll), 6 Fatal (error parked in ctx). A
callee SideExit makes the caller run `jit_deopt_stack_call` (interpreter
finishes the callee); the caller itself never bails. No native unwinding.

## Tier-up
Caller-side: `CodeEntryCell.generated_entries++`, compared with
`generated_tiering_break_even`, `enabled`, and the `hot_function` mailbox
(write callee id + 1). Drained by `promote_hot_generated_callee` and at the
outermost return by `reconcile_generated_feedback`.

## Deopt
Machine exit dumps x0–x29/d0–d15, `jit_deopt_writeback_stub` rewrites the
frame's window from `DeoptRuntime` frame states and returns SideExit; it
relies on function_id/flags/register_base/register_count/this/self/
new_target/argument_count/arguments_object being written eagerly at call.

## Stack walkers
`snapshot_active_frames` walks the activation array (VM-created errors);
`snapshot_frames` (native `Error()`, `Error.captureStackTrace`, profiler,
async provenance) sees only materialized frames — generated frames are
missing there.

## Redesign notes
1. Machine callers do not stamp `pc` before arm64 direct calls: stack
   snapshots show a stale pc.
2. `HAS_SAFEPOINTS` has no reader; `kind` one reader.
3. Safepoint identity is dynamic (published per call); nothing keyed by
   return address.
4. Machine callee windows are traced for the whole call although only deopt
   writes them.
