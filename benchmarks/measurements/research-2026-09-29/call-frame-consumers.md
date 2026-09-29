# Call/frame contract consumer map (2026-09-29, HEAD de0784c6)

Read-only map of every reader and writer of `NativeFrame`, the activation
array, Machine root records, `JitCtx`, `VmThread` current-frame fields,
`NativeResultPair` statuses and `CodeEntryCell` counters. One reader per
area plus a completeness critic. Paths are relative to `crates/otter-jit/src`
or `crates/otter-vm/src` unless given in full. Input for the redesign in
`../2026-09-27-architecture.md` section 15.

## Template→Template direct call on arm64 (Plain `Op::Call`, strict callee, fixed args)

Call site: `template/arm64.rs:958-984` → `calls::emit_call` → `emit_call_with_receiver` (`calls.rs:1127-1158`) → `arm64/direct_call.rs::emit_direct_call` (`:640-698`, context_register=20) → `emit_direct_call_with_access` (`:712-1513`). Labels: bail=`identity_guard_exit`, transition=`runtime_transition_exit`, finish_error=`threw`, throw_value=`committed_throw`.

| # | Step | file:line | instrs |
|---|---|---|---|
| 0 | PC stamp `movz w9,pc; str w9,[x21,#4]` | arm64.rs:341-342, 2076-2079 | 2 |
| 1 | Activation-array capacity check | direct_call.rs:771-778 | 5 |
| 2 | Callable guard (fn-id immediate / closure path) | :803-844 | 6 / ~22 |
| 3 | Strict `this`=undefined or bound-this; sloppy global load :457-477 | :846-865 | 1-6 (+~6) |
| 4 | `sub sp,#frame_bytes`; `str x25`; SELF; this/new_target `stp` | :991, 998-1021 | 5 |
| 5 | Argument copy | :343-375, 1035 | 2·min(A,P) |
| 6 | Entry cell movz/movk, add, `ldar x25`, cbnz (cold resolve stub), str target_cell, ldr frame bytes, cbz | :1040-1073 | ~10 |
| 7 | Native stack check + entry addr load + header copy | :1074-1093 | 11 |
| 8 | Header word, register_base, argument_count (64-bit store zeroes arguments_object) | :1099-1117 | 5 |
| 9 | Window fill undefined (PARAMETER_PREFIX flag skips locals) | :1136-1153, 378-421 | ~4 + ⌈(R−A)/2⌉ |
| 10 | Save caller frame + code id in linkage, activation push, `ctx.native_frame ← sp`, `VmThread.current_frame/code_object_id` stp, ldr entry | :1438-1457 | 15 |
| 11 | Tier counter `generated_entries++`, break-even cmp, mailbox | tiering.rs:19-37 | 6 / ≤14 |
| 12 | `str xzr,[ctx,feedback_clean]; mov x0,x20; blr x16` | :1469-1474 | 3 |
| 13 | Callee prologue (stp x29/x30 −48, stp x19/x20, str x21, mov x29,sp, mov x20,x0, ldr x21 native_frame, ldr x19 register_base) | arm64.rs:2029-2040 | 7 |
| 14 | Callee `movz x1,Success` + epilogue | arm64.rs:1738-1743, 2044-2052 | 5 |
| 15 | Status dispatch | completion.rs:66-82, 159-161, 218 | 6 |
| 16 | Cleanup: restore ctx.native_frame + VmThread, activation pop, ldr x25, add sp, cmp | completion.rs:221-249 | 16 |
| 17 | `str x0,[x19,dst]; b done` | completion.rs:250-253 | 2 |

Total ~110-135 instrs (fib-like). Publication alone (0,1,10,16) ≈38.

### Frame layout
- Template frames ARE fp-chained: `stp x29,x30,[sp,#-48]!`… `mov x29,sp` (arm64.rs:2032-2035); x19/x20 at fp+16, x21 at fp+32 (`NATIVE_FRAME_BYTES=48`, arm64.rs:114).
- Caller allocates callee frame sp-relative (layout.rs:77-100): sp+0 NativeFrame (56 padded 64, entry/abi.rs:289); +64 saved_x25, +72 entry_addr, +80 caller_frame, +88 caller_code_object_id, +96 target_cell; window from +104; ≤4080 16-aligned.
- Nested Template callee: NativeFrame = callee fp + 48; return address [fp+8].
- Outermost NativeFrame is a Rust local (`entry/code.rs:101`), register_base on interpreter stack.
- x25 = generation handle across call (completion.rs:79, 181, 241), must be callee-saved.

### PC stamping / exits
- Every non-pure op stamps pc (arm64.rs:338-343; exemptions 1949-1976). Shared side-exit epilogues read `ldr w0,[x21,#pc]` (2054-2073). Exception helper rewrites pc to handler (exceptions.rs:65-67).

### Runtime stubs
`mov x0,x20`, movz/movk abs addr x16 (values.rs:73-87), blr, status chain (transitions.rs:45-91). Stubs find frame via `ctx.native_frame`. Alloc stubs push `AllocCtx{thread,safepoint_id,spill}` via transient `sub sp` (transitions.rs:497-525); value packets too (value_packet.rs:59-87). Generic call `STUB_JIT_CALL_WITH_THIS_VALUE` (calls.rs:1218-1247).

### Callee SideExit / Throw
- SideExit: caller checks payload low32 == callee `[sp+4]` pc else Fatal; `generated_deopts++`; `STUB_JIT_DEOPT_STACK_CALL(ctx, sp, caller_fn, logical_pc, callee code id, caller code id, form)` (completion.rs:162-217).
- Throw: x0=value, x1=Throw (arm64.rs:1855-1857); `generated_throws++`, cleanup, `b committed_throw` → `STUB_JIT_ROUTE_THROW` (arm64.rs:1801-1823, completion.rs:254-268).

### x86_64
Separate emitter `template/x86_64/direct_call.rs:797-1110`; rbp-chained (x86_64.rs:1704-1717), NativeFrame at rbp+16; fills ALL R registers (1198-1202); guards callable twice (875, 935); reloads r14/r13 after return (1066-1067).

### Hazards
1. Callee prologue finds frame via `ctx.native_frame` (arm64.rs:2037; x86 1714) — publication at direct_call.rs:1453 is ABI. Must become `add x21,x29,#48` or register arg; outer + OSR entries (entry/code.rs:101; arm64.rs:1885-1897) need same shape.
2. Shared side-exit epilogues take pc from frame (arm64.rs:2064); caller cross-checks (completion.rs:163-166). Exits still need stamp or per-site stubs.
3. Template callees read/write window directly; GC traces whole window via activation array. Skipping undefined fill needs liveness stack maps or callee init.
4. Caller owns/sizes callee frame (layout.rs); stack check covers only callee's 48-byte prologue (code.rs:148-156; direct_call.rs:1079-1083).
5. Tiering reads `function_id` from `[sp]` (tiering.rs:33).
6. Deopt/throw completion needs eager frame fields + linkage slots (completion.rs:174-182).
7. Machine frames save x29 but never set it → Template→Machine→Template walk breaks. x25 callee-saved in all tiers.
8. Transient `sub sp` packets OK for fp walks; direct-call assumes sp == body base at call site.
9. `VmThread.current_frame/code_object_id` stored per call (direct_call.rs:1455, completion.rs:230); need pc→code lookup before removal.

## Machine arm64 Machine→Machine direct call (Plain, strict, fixed args). Prologue at arm64.rs:5216-5253.
A=`machine/numeric/arm64.rs`, D=`arm64/direct_call.rs`, C=`arm64/direct_call/completion.rs`, T=`direct_call/tiering.rs`, L=`direct_call/layout.rs`. R roots, P params, L locals, g saved GPRs. ~110 before blr, ~15 callee frame, ~30 after.

| # | Step | file:line | instrs |
|---|---|---|---|
| 0 | regalloc2 edits Before(call) | A:5052-5114 | var |
| 1 | Roots → root-save homes | A:3584 → A:4764-4786 | R..2R |
| 2 | Push JitMachineRootRecord (sub sp 32; prev/base/count/safepoint_id/code id; head swap) | A:3585 → A:4857-4896 | 13 |
| 3 | Inline-parent publication | A:3592, inline_calls.rs:69 | 0 for fib |
| 4 | Activation cursor limit | D:771-778 | 5 |
| 5 | Callable guard (reload from root home A:3784-3796) | D:808-845 | ~5 / ~20 closure |
| 6 | Strict receiver / bound-this (sloppy +~7 global D:457-476) | D:846-865 | ~4 |
| 7 | Reserve linkage, save x25, SELF, this/new_target | D:991-1022 | 5 |
| 8 | Args → callee window | D:343-375 | 2P |
| 9 | Entry cell ldar/cbnz | D:1040-1056 | ~7 |
| 10 | Target cell, frame-bytes check, stack limit, entry addr, header word 0 | D:1070-1093 | 14 |
| 11 | Header word, register_base, argument_count | D:1105-1117 | 5 |
| 12 | Fill locals undefined (skip only PARAMETER_PREFIX) | D:1143-1153, D:378-421 | 4+⌈L/2⌉ |
| 13 | Save caller frame/code id, push Array, ctx.native_frame, VmThread current_frame + code id | D:1438-1457 | 17 |
| 14 | Tier counter | T:19-37 | 6-8 |
| 15 | zero feedback_clean, mov x0,x19, blr | D:1469-1474 | 3 |
| 16 | Callee prologue stp x29/x30, stp x19/x20, x21+ pairs, d8+, sub sp, mov x19,x0, ldr x17 native_frame, ldr x17 register_base | A:5216-5253 | ~7+⌈(g−1)/2⌉ |
| 17 | EntryValue = ldr from in-memory window | A:1335-1341 | P |
| 18 | Return mov x0; movz x1 Success; epilogue; ret | A:3373-3381, A:5451-5483 | ~8 |
| 19 | Status dispatch | C:66-76, 159-161, 218 | 6 |
| 20 | Cleanup restore ctx/current_frame/code id, pop Array, reload x25, add sp, cmp | C:222-249 | 17 |
| 21 | restore_roots: pop record (4) + reload roots | A:3815-3824, 4898-4906, 4820-4847 | 6+R..2R |
| 22 | store dst, b done | A:3809-3814, C:252-253 | 2 |

- No pc stamp on arm64 (x86 stamps: x86_64/direct_call.rs:481-483). x86 publish 727-744, status 745-754.
- Stack: caller Machine frame | 32 B root record | linkage frame [NativeFrame 64][saved_x25, entry_addr, caller_frame, caller_code_id, target_cell][window][actual args] (L:78-102) | callee Machine frame.

### Safepoints
- SafepointId dense per code object, instruction order (safepoint.rs:164-170).
- MachineSafepointRoot{value, source: AllocatedLocation, save_slot} (safepoint.rs:43-63); aliases saved too (Ion populate rule, 16-23, 206-221).
- SafepointRecord.tagged_locations = spill_slot(0..count) — indexes root-save area, NOT regalloc2 spill slots (225-234).
- regalloc2 0.15.1 no reftype/stackmap: roots are late-use metadata operands (regalloc.rs:357-367). regalloc2 spill slots + callee-saved regs never GC-visible; copied into homes + reloaded.
- Frame layout from post-prologue sp: [regalloc spills][root homes][raw scratch: packets, inline NativeFrames] (frame.rs:10-16, 108-149), then saved d8+, x21+, x19/x20, x29/x30. Fixed 32 base (target.rs:28, A:204-212). generated_stack_frame_bytes = frame_bytes + max(deopt dump 46×8, root record) (A:174, 4640-4664).
- DirectCall clobbers arm64 x0-x14, d0-d7 (target.rs:309-314, 366); values across calls in x20-x28, d8-d15 or spill. x25 linkage scratch (D:1000, C:241). Allocatable x0-x14, x20-x28, d0-d15; scratch x15-x18 (target.rs:193-206).
- GC tracing walks root-record chain ignoring id (vm interp/frames.rs:289-310); generated_caller_matches reads roots.safepoint_id (generated.rs:53-75).

### x29, OSR, window
- x29 saved never set as fp; w29 = backedge poll countdown (A:57-59, 248-250, 1220). x86 same: push rbp w/o mov rbp,rsp; ebp poll counter (x86_64.rs:205, 2955, 3566-3568).
- x30 far-offset scratch in body (A:5539-5550). Deopt dump keeps unused x29 slot (A:169-170, 4570).
- OsrDispatch reads OSR_ENTRY + pc from ctx.native_frame (A:1275-1316); OsrValue via register_base (A:5288-5296).
- PARAMETER_PREFIX only when no safepoints and no backedge frame states (mod.rs:414-418) ⇒ recursive fib full-fills locals every call. Prefix widened cold by emit_materialize_vm_window (A:4982-5007).

### Hazards
1. x29/rbp taken by poll counter.
2. Roots in callee-saved regs (x20-x28 / rbx,r12-r14): stack-map walk needs callee-save tracking per frame (JSC) or direct-call clobbers cover all allocatable regs (V8). regalloc2 has no stackmap API: derive spill-slot liveness per return address from metadata operands.
3. Callee depends on ctx.native_frame: prologue register base (A:5250), EntryThis (A:1346), generic-call pc stamping (A:3960), OSR, materialize.
4. Params passed in memory (A:1340); deopt writeback + stack-call deopt need window + header (C:163-214).
5. MACHINE_ROOT_RECORD_SIZE bias baked in in-call root/raw offsets (A:3790, 3845, 3857, inline_calls.rs:28-34).
6. Frame budget caller-side (D:1071-1083).
7. Status checked after every blr (C:66-76).
8. Activation cursor = recursion bound (D:771-778; D:53-56).
9. Inline parents published into Array + ctx.native_frame per call (inline_calls.rs:78-158).

## GC roots in generated frames (paths under crates/)

### Consumers
| Site | Reads/writes | Invariant | Needs under fp-walk |
|---|---|---|---|
| otter-vm/src/runtime_state.rs:218-221 (from visit_extra_roots lib.rs:1922-1924) | trace_reg_stack then trace_native_jit_activations | arena windows traced once by reg stack; stack windows only via published frames | walk from innermost |
| otter-vm/src/interp/frames.rs:274-288 | per Array entry ActiveFrameRef::from_native_ptr (active_frame.rs:255); STACK_REGISTERS ⇒ whole window + incoming args (active_frame.rs:421-435), self/this/new_target, arguments_object (444-470) | window[0..register_count] + incoming valid for whole call (checked_register_windows active_frame.rs:162-195); rewritten in place | fp walk, NativeFrame at fixed fp offset; per-ret-addr liveness |
| frames.rs:289-310 | jit_machine_roots chain (previous, root_base, root_count), rewrite homes; safepoint_id/code id only debug_assert (:299-300) | records live until restore; depth < capacity (:292-295) | homes from fp + stack map at ret addr |
| otter-vm/src/runtime_stubs.rs:360-383 alloc_safepoint_record | (current_code_object_id, safepoint_id) → jit_registry.rs:812-826 | VmThread current code id/frame exact (arm64/direct_call.rs:1440-1455, completion.rs:228-238, x86 machine direct_call.rs:725-736) | key by stub ret addr |
| runtime_stubs.rs:476-510 AllocSafepointFrameRoots | FrameSlot (Template current_frame.register_base) + SpillSlot (Machine ctx.spill_slots); bounds debug only (:393-436, :462) | current_frame = stub caller | subsumed by walk |
| otter-vm/src/root_census.rs:332-335 | counts | | diagnostic |
| otter-gc/src/heap.rs:2381-2392 finish_incremental_mark_phase | re-scan all roots | frames have NO write barrier (e.g. TryBindDerivedThis arm64.rs:1418-1435) | walk at final re-scan |

### Producers
- Machine root record push arm64 emit_publish_machine_roots_with_bias arm64.rs:4857-4896 (sub sp 32, link, site.id immediate, code id from VmThread); pop emit_clear_machine_roots :4898-4906. Sites: committed :466, :642; direct :3585; guard-miss re-publish :3906-3907; clears :518/530/546/549/689/3820/3975/4009/4017; arm64/forward_call.rs:85,151.
- x86 publish_roots/clear_roots x86_64.rs:4197-4245; used x86_64/direct_call.rs:175,261,386,432,478,1006.
- Root spill/reload: arm64 emit_save_safepoint_roots :4764, emit_reload_safepoint_roots_with_bias :4820-4847 (stack-slot root copied back into its regalloc spill slot :4834-4839); x86 save_roots :4315, reload_roots :4337. Homes at MachineFrameLayout::root_offset (machine/frame.rs:192-201).
- Alloc stubs (no record): arm64 emit_allocating_call :4700-4760; x86 allocating_call :4272-4310; RuntimeStubAllocContext{thread, spill_slots, safepoint_id, count}; Template template/x86_64/context.rs:287-292.
- safepoint_id: CFG builders (native_call_cfg.rs:652, element_cfg.rs:414, property_cfg.rs:131); renumbered machine/mod.rs:667-676; density safepoint.rs:176-183; record per site tagged_locations=spill_slot(0..n) (:228-235); aliases (:209-222).
- Activation publication: arm64 push direct_call.rs:1445-1453, depth check :773-777, pop completion.rs:231-238. Window fill emit_initialize_register_range :377+; incoming args :1119-1133. Window initially param prefix (code_entry.rs:157-167). Inline parents: inline_calls.rs:59-192 (raw-area copies, rooted only while published); x86 x86_64/inline_calls.rs:81-188. Rust: entry/code.rs:151-156 (arena, not STACK_REGISTERS, OSR_ENTRY :97-99); inline_activations.rs:83-160; jit_push_native_frame frames.rs:172-199.
- Non-GC record consumers: inline_frames.rs:20-55; generated_caller_matches generated.rs:54-120.
- Vestigial: SafepointEntry.native_return_offset (native_abi/safepoints.rs:152-167); deopt::StackMap (deopt.rs:710) keyed by byte pc.

### Hazards
1. Machine not fp-chained; stub frames (Rust extern "C") need entry fp anchor + exit fp at stub transition.
2. Moving GC rewrites homes in place, code reloads; ret-addr map must name same homes incl aliases (safepoint.rs:9-24).
3. Whole window traced; Machine callees not initializing ⇒ need per-ret-addr liveness; deopt widening needs story.
4. Inline parents in raw area disappear ⇒ walk from SafepointRecord.inline_frames.
5. STACK_REGISTERS double-trace guard: outer entry + OSR arena frames excluded.
6. Array cursor non-GC uses: retirement epoch (jit_call.rs:1318-1325, jit_compile.rs:577-580), recursion budget (frames.rs:234-268, direct_call.rs:773-777), root-chain bound assert (frames.rs:292), snapshot_active_frames, inline_activations base check.
7. x86 scratch reuse: record root_base/code_object_id hold returned payload/status (x86_64/direct_call.rs:1398-1408, 1759-1801).
8. current_code_object_id/current_frame stored per call only for alloc stub + inline decode; ret-addr map must cover invalid code until retirement (jit_registry.rs:818-820).
9. generated_caller_matches requires caller record = chain head at callee deopt.
10. Incremental marking relies on re-scan instead of barriers.

## Deopt frame-state map

### Producers (compile time)
- `machine/numeric/frame_state.rs:73-96` resume_pc/suspended_call_pc.
- `machine/deopt.rs:91-212` lower_deopt_table: DeoptLocation::{Register(gpr / gpr_budget+fp), StackSlot(sp-relative), Literal, VirtualObject}; FrameStateIds dense (117-122); one physical instruction per state (99-113 AmbiguousExitState).
- `numeric/mod.rs:445-492` DeoptRuntime{table, exits[DeoptExitDescriptor{state,reason,action,resume_pcs}], gpr_budget}. Exit ids dense/unique (475-486), renumbered after GVN (gvn.rs:751-757), allocated mod.rs:4670. Nothing keyed by return address. Exit index `movz w17, idx` u16 (arm64.rs:4557-4566). DeoptRuntime address baked via RelocationTarget::DeoptRuntimeData (arm64.rs:4602-4608), lives with code (optimizing/mod.rs:80, deopt.rs:667-676).

### Eager exit handler
- arm64 shared handler (arm64.rs:4569-4627): push x0..x28 + xzr hole for x29, d0..d15; x3=dump, x4=sp+DUMP, x5=[[x19+NATIVE_FRAME]+REGISTER_BASE].
- x86 (x86_64.rs:4466-4508) 16 GPR + 16 xmm, r9 window.
- `jit_deopt_writeback_stub` (entry/runtime_ops/reentry.rs:221-467) single frame: reads ctx.native_frame function_id (==outermost, 290), STACK_REGISTERS (299); non-stack-owned → rebuild handlers via materialized_frame_index (307-332); writes register_count (widened, 342), window (343,391), pc (393); returns SideExit(resume_pc). Virtual-object materialization allocates (374-376) with window partly written — GC-safe only because frame stays in Array and window traced.
- Multi-frame (inline): decode (402-432), jit_deopt_materialize_inline_frames (456), returns Success(value) — whole chain completes in interpreter.
- Entry bail widens param-prefix window (arm64.rs:4508-4515, emit_materialize_vm_window 4982-5006; x86 4438-4461), pc=0. OSR bail stamps pc (arm64.rs:4630-4643).

### Caller-side SideExit
- arm64/direct_call/completion.rs:66-69 status 1 → callee_bailed; 162-166 low32 == [sp+PC]; generated_deopts++ (170); STUB_JIT_DEOPT_STACK_CALL(ctx, sp, caller fid, call pc, [x25].code_object_id, [sp+caller_code_object_id], kind) (171-203); Success=normal return (205-206). Cleanup 218-230.
- Same pattern: x86 Machine direct_call.rs:843,1238; Template calls.rs:1149,1360,1669; forward_call.rs:143; spread_call.rs:127; template/x86_64/direct_call.rs:742,1045.
- jit_deopt_stack_call_stub (entry/runtime_ops/calls.rs:65-122) checks pc (84).

### VM consumers
- note_generated_call_deopt (interp/jit_calls/generated.rs:117-199): STACK_REGISTERS (128-135); fid + kind vs registry generated_deopt_state (147-155) — kind HAS reader; pc (163); bail policy (180-197).
- generated_caller_matches (generated.rs:35-114): jit_machine_roots code id + safepoint_id (53-70); walks Array[top-len-2..] (77,86,100-109).
- jit_deopt_materialize_stack_call / resume (interp/jit_calls/deopt.rs:60-182): fid (73,111), STACK_REGISTERS (76-82), pc (116,137), register_count == function.register_count (118-121), register_base, self/this (127-128), arguments_object (138), INCOMING_ARGUMENTS+argument_count + incoming window at register_base+register_count*8 (144-149; active_frame.rs:162-195), new_target (157,165). Copies into arena Frame; with_materialized_native_frame (interp/host.rs:367-381) records (native addr, index); consumed by jit_generated_call_depth (frames.rs:258-268), snapshot_active_frames (native_stack_snapshot.rs:30-40); dispatch_loop_above_rooted.
- jit_deopt_materialize_inline_frames (deopt.rs:192-411): outer fid (206), register_base (245-247), incoming args (261-268), this/self/new_target defaults (309-314), arguments_object (339), derived flag (359).
- interp/restore.rs = isolate snapshot restore, unrelated.

### Inline-frame materialization (non-deopt)
- Machine direct construct: arm64/inline_calls.rs:60-159 physically publishes inline parents: stamps outer pc (86-88), header writes (106-144), Array push (145-151), ctx.native_frame + current_frame (154-157); leave 161-186.
- Committed runtime calls: entry/runtime_ops/inline_frames.rs:20-102 recipe from ctx.machine_roots + safepoint_id via registry; with_inline_activations (runtime_activation/inline_activations.rs:70-176) requires activations[top-1]==self.frame (84); boxed NativeFrames pushed (130-158); write back this/self/new_target after GC (46-55). Callers vm_ops.rs:69,101; reentry.rs:1054. Verifier machine/inline_frames.rs:14-108.

### Eager field requirements
- At exit only: pc, widened register_count.
- Valid when deopt runs: function_id, STACK_REGISTERS, register_base, self/this/new_target (callee entry bindings not in deopt data), argument_count + INCOMING_ARGUMENTS, arguments_object (needs zero-init), kind.
- Could come from caller deopt data at return address or fp.

### Invalidation
- No lazy deopt. invalidate_dependents/invalidate_code_objects (jit_registry.rs:532-560,792-809) only unlink CodeEntryCell (code_entry.rs:224-227). Active frames rely on inline guards (protector load arm64.rs:2464-2473). Invalid code still resolves safepoints (jit_registry.rs:811-820).

### Hazards
1. Writeback finds frame via JitCtx.native_frame (arm64.rs:4613, reentry.rs:287,339,435).
2. generated_caller_matches + with_inline_activations index Array (generated.rs:74-109, inline_activations.rs:83-87).
3. Depth/stack-overflow (frames.rs:258-268) + snapshot are Array-based.
4. GC during virtual-object materialization assumes frame published (reentry.rs:374, deopt.rs:31-33).
5. Id namespaces: exit index (u16), FrameStateId; no call-site frame state today (caller never deopts itself).
6. Param-prefix frames: register_count changes mid-frame (reentry.rs:342); checked_register_windows derives incoming base from it (active_frame.rs:182-188).
7. x29 dump slot hole (arm64.rs:4583) — keep x29 non-allocatable.
8. Inline byte→pc scan linear (deopt.rs:232-235, inline_activations.rs:118-121), cold.
9. Template relies on live window, stamped pc, same caller-side stub.

## Stack traces, profiler, introspection
Paths under `crates/otter-vm/src` unless `jit/`. "Array" = `jit_native_activations[..top]`; "Mat" = materialized ActivationStack.

| Consumer | file:line | Fields | Depends on |
|---|---|---|---|
| `snapshot_active_frames` | native_stack_snapshot.rs:22-99 | per array entry: function_id, pc (exact, no −1, :83), STACK_REGISTERS, register_base; finds Mat owner by fid+register_base (:42-53); `jit_materialized_generated_calls` (:36-40) | Array + eager pc; Machine arm64 direct call leaves pc STALE |
| VM-created errors | error_ops.rs:113 (:95, :538); RuntimeCall NewError committed_values.rs:583 | via snapshot_active_frames | Array + pc |
| Uncaught-throw provenance | dispatch.rs:758, :781; committed_values.rs:556 PrepareThrow | via snapshot_active_frames | Array + pc |
| `snapshot_frames`/`visit_frame_snapshots` | stack_snapshot.rs:43-77 | Mat fid, pc−1 | generated frames never appear |
| Native `Error()` | error_classes.rs:1014 → capture_active_frames runtime_cx.rs:392-397 | snapshot_frames | Mat only (gap) |
| `Error.captureStackTrace` | error_classes.rs:1328-1402; frames_above_callee error_ops.rs:618-624 | Mat self_value | Mat only; 2 bugs: generated constructorOpt not found; skip counted in Mat index space |
| Compiled-throw provenance | jit_exception_ops.rs:196; jit_call.rs:94 | snapshot_frames | Mat only (differs from PrepareThrow) |
| Run-level/async errors | exec.rs:920,1060,1157,1190; async_ops.rs:271,301,377,445,459,475 | snapshot_frames | Mat |
| CPU profiler | cpu_profile.rs:214-240, sampled only dispatch.rs:232 | Mat | generated code invisible |
| `fn.caller` | function_ops.rs:2408-2440 | Mat self_value, fid | generated → null/wrong |
| `fn.arguments` | function_ops.rs:2449+ | Mat | misses generated |
| Strict lookup | property_dispatch/drivers.rs:632-639 (:685,:732,:958) | stack.last().function_id | relies on force_strict |
| Direct eval | eval_ops.rs:92-105,164-182 | Mat top | JIT Op::Eval side-exits unless ctx.native_frame lacks STACK_REGISTERS (jit/entry/runtime_ops/reentry.rs:684-686, jit/entry/abi.rs:129-144) |
| Realm by frame | global_ops.rs:52, init.rs:1151 | current fid | current only |
| RuntimeCall identity | runtime_activation/mod.rs:127-135 bind, :188 function_id(), :195 pc(), forward_arguments.rs:116 | current NativeFrame header | eager pc |
| `generated_caller_matches` | interp/jit_calls/generated.rs:36-112 | Array tail: callee identity (:79), root fid (:89), each inline parent fid/pc/register_count/STACK_REGISTERS, last parent pc==call_pc (:100-108); jit_machine_roots.safepoint_id (:55-67) | Array positional + stamped pcs + dynamic safepoint id |
| Generated-call deopt | jit/entry/runtime_ops/calls.rs:65-118; interp/jit_calls/deopt.rs:73-137,:171 | callee header.pc == side_exit.logical_pc (calls.rs:84) | pc stamp; with_materialized_native_frame (interp/host.rs:367-381) keyed by NativeFrame address |
| Inline frames in traces | jit/entry/runtime_ops/inline_frames.rs:20-40; inline_activations.rs:70-160 | roots.safepoint_id + thread.current_code_object_id → registry inline_frames; synthesized NativeFrames pushed into Array | used by load/store-property + binding stubs (vm_ops.rs:69,:101; reentry.rs:1054) |
| VmThread.current_frame/code id | set jit/entry/code.rs:117-118; swapped jit/arm64/direct_call.rs:1455, completion.rs:230, x86 :696/:753; read native_abi/runtime_stubs.rs:374-389, runtime_stubs.rs:403,:480 | | per-call store |
| Array-top liveness | interp/jit_call.rs:1318; jit_compile.rs:579,:833 retire only top==0; jit_generated_call_depth frames.rs:255-265 | top | retirement + recursion bound |
| `--jit-events` | jit_debug.rs:426+ (:651,:670,:678; generated.rs:162-167) | static pcs, exit payload | no walk |
| console.trace | console.rs:142-150 | none | prints no frames |

### Needs under fp-walk
1. snapshot_active_frames: fp iterator giving NativeFrame ptr, code id, return address; pc from table (ret addr → fid, logical call pc, inline chain). Top frame pc at stub entry from stub return address or static (fid,pc) in PropertySourceCell (vm_ops.rs:64-66). Interleave with Mat via entry-frame marker.
2. Inline frames from ret addr → safepoint → inline_frames; remove with_inline_activations publication (inline_activations.rs:83-160) + dynamic safepoint_id (inline_frames.rs:33).
3. generated_caller_matches over walked parents.
4. Error(), captureStackTrace, fn.caller/arguments, profiler, compiled-throw provenance → unified walker (fixes Mat-only gaps). Walked frame must expose SELF (+0x28).
5. Retirement + depth: entry-depth counter per outer entry + byte stack limit.

### Hazards
- Stale parent pcs TODAY (Machine arm64 no stamp) → wrong VM-error stacks (native_stack_snapshot.rs:65/75).
- Top pc cannot come from ret-addr table: RuntimeCall::pc() (mod.rs:195) operand decode, set_pc (mod.rs:201), calls.rs:84 check.
- Inconsistent provenance paths (jit_call.rs:94, jit_exception_ops.rs:196 vs PrepareThrow).
- Deopt transfer keyed by NativeFrame address (host.rs:376).
- GC tracing uses same Array (frames.rs:273-285).
- Strict/eval rely on STACK_REGISTERS + force_strict; keep current-frame register so materialized_frame_index (jit/entry/abi.rs:129-144) stays precise.

## Runtime stubs from generated code
JitCtx is 104 B (entry/abi.rs:305): 0x40 feedback_clean, 0x48 machine_roots, 0x50 alloc_window, 0x60 runtime_stats.

### Finding the frame
- JitCtx::runtime_call (entry/abi.rs:75-90): thread->runtime_context (VmRuntimeActivation) + JitCtx.native_frame → RuntimeCall::bind.
- RuntimeCall::bind (runtime_activation/mod.rs:103-162): validates window (ActiveFrameRef::from_native_ptr :116); reads flags, fid, register_count, register_base (:122-131); non-STACK_REGISTERS must equal ActivationStack[frame_index] (:136-147); resolves + CLONES ExecutionContext for fid (:150-153); pc()/set_pc() (:193-201); every with_frame re-validates (:276-284).
- Alloc stubs: RuntimeStubAllocContext reads VmThread.current_frame + current_code_object_id (native_abi/runtime_stubs.rs:375-389).
- materialized_frame_index (abi.rs:129-144).

| Family | Where | Frame fields | Status | Reentry |
|---|---|---|---|---|
| Variadic materialized-only (iterator, static-call, spread, class, module) | reentry.rs:538-790 | materialized_frame_index; nested generated callee → pre-effect SideExit (:594-596, :633-635) | status word x0 (:613-620); Template decode transitions.rs:45-71 | jit_runtime_*_op → run_callable_sync_rooted (jit_spread_call_ops.rs:200,697; call_ops.rs:3394-3416) |
| Variadic via RuntimeCall (make_fn, regexp, collect_arguments, literals) | vm_ops.rs:246-300, literals.rs:20-86 | window by index; fid | status word or Probe/Committed | alloc only |
| Committed pair (CommittedValue2, ReentrantNamedLoad/Store, ReentrantValueSpan) | vm_ops.rs:55-227, forward_arguments.rs:75-100 | named ops (fid,pc) from PropertySourceCell (vm_ops.rs:64-67); call ops decode opcode from frame pc (value_ops.rs:159-176); inline_frames::decode root head + current_code_object_id → registry.resolve (inline_frames.rs:20-50) | x1 0/2/6 (reentry.rs:89-106) | packet → SmallVec (vm_ops.rs:132-146) |
| Alloc (AllocValue3, 12 stubs) | runtime_stubs.rs:357-520 | thread.current_frame register_base/count for FrameSlot; packet spill slots (:476-507); safepoint (current_code_object_id, packet.safepoint_id) (:378) | Probe incl OOM | none |
| Leaf | native_abi/runtime_stubs.rs:28-60 | none | value/status/f64 | none |
| Backedge poll | reentry.rs:1583-1604 | try_runtime_call | Success/Yield/SideExit/Throw | none |
| Throw routing | route_throw reentry.rs:126-141; finish_error :477-495; RuntimeCall::route_throw exceptions.rs:103-134 | fid+pc select catch; WRITES exception reg + header.pc | SideExit(catch pc)/Throw/Fatal | jit_route_throw |
| Linkage helpers (forward plan/copy, deopt_stack_call, push/pop) | forward_arguments.rs:29-72; calls.rs:29-121 | callee frame explicit x1=sp; caller ids immediates/slots (completion.rs:172-203) | u64::MAX miss; pair | jit_deopt_materialize_stack_call |
| Deopt writeback | reentry.rs:221-396 | writes register_count, pc | SideExit | none |

### Error slot 0x10 / feedback_clean 0x40
- Error slot → Option<VmError> on outer Rust entry stack (code.rs:128-130), shared per JitCtx (abi.rs:42-45). park_jit_error (reentry.rs:53-60); consumed by jit_finish_error_stub (template/arm64.rs:1833-1853) or outer exit (code.rs:188-190).
- feedback_clean: set 1 at outer entry (code.rs:145); zeroed by every linkage (direct_call.rs:1471, runtime_forward.rs:256, x86 :421); read at outer exit (code.rs:168) → jit_generated_feedback_pending (jit_call.rs:1317) → reconcile. Unrelated to frames.

### Caller-side costs (static counts)
- Machine committed call (arm64.rs:473-548): root record push 13 (:4863-4893) + clear 4 (:4898-4905); pc stamp 4 (:507-512); movz/movk stub addr; blr; cbz x1 + 2 cmp (:518-525); +1-2/root save+reload. ≈30 fixed + 3/root.
- Template transition: pc stamp 2, mov x0,x20, addr, blr, 4-6 ladder ≈12.
- Rust side: runtime_call + bind (window validation, ActivationStack cross-check, for_function, ExecutionContext clone), re-validation per with_frame — likely dominant.
- Runtime forward linkage publication ~17 (runtime_forward.rs:230-257) + ~14 restore (completion.rs:222-237); x86 same (x86_64/runtime_forward.rs:399-422, 482-495).

### Needs under fp-walk
1. One "last exit frame" slot on VmThread written only at runtime transitions (V8 c_entry_fp / JSC topCallFrame); runtime_call, alloc current_frame, inline decode derive frame as fp−const. Replaces JitCtx.native_frame + VmThread.current_frame.
2. pc + safepoint from return address table: (code object, logical pc, safepoint id, inline chain). Machine inline sites publish PARENT pc (arm64.rs:585-590).
3. Materialized ops need outer-entry test: keep STACK_REGISTERS in header or ret-addr metadata.
4. Status stays for Committed/Probe/StatusWord; ladders → cbz fast path.

### Hazards
- Stubs WRITE frame: route_throw pc (reentry.rs:135), deopt register_count/pc (:342, :393), RuntimeCall::write registers. Written pc must win over ret-addr mapping for resume.
- Alloc stubs trace thread.current_frame window (runtime_stubs.rs:490-507).
- run_callable_sync_rooted → interpreter → new enter_compiled builds new JitCtx + error slot (code.rs:117-160); walk can't follow Rust frames ⇒ entry record chaining to previous exit fp; parked errors must not cross entries.
- bind cross-checks ActivationStack (mod.rs:142-147).
- Template stamps pc before every exiting op; dropping requires ret-addr decode in every stub.

## Exceptions and unwinding
No native unwinding: throw = return status 2 + value in x0/rax; every frame checks status, cleans own linkage, catches or returns Throw.

| Step | Mechanism | Cite |
|---|---|---|
| Status alphabet | 0 Success,1 SideExit,2 Throw,3 Continue,4 OOM,5 Yield,6 Fatal | otter-vm/src/native_abi/dispatch.rs:185-202 |
| Error slot 0x10 | *mut Option<VmError> → Rust local in enter_compiled, shared by nested callees | entry/abi.rs:42-45, entry/code.rs:131-141 |
| Status-word stubs | park_jit_error, Throw → `threw` → STUB_JIT_FINISH_ERROR (take_js_throw consumes pending_uncaught_throw on VmError::Uncaught, route_throw_value) | reentry.rs:53-60,477-491; committed_values.rs:661-679; template/arm64.rs:1832-1857 |
| Pair stubs | Throw → committed_throw → STUB_JIT_ROUTE_THROW | transitions.rs:379-385; template/arm64.rs:1801-1822 |
| Throw op | PrepareThrow → committed_throw | template/arm64.rs:1137-1151; template/x86_64.rs:1288-1300 |
| Template try/catch | EnterTry/LeaveTry/EndFinally/JumpViaFinally/PopParkedFinally/TdzError each blr STUB_JIT_EXCEPTION_OP + 4-way dispatch | template/arm64/exceptions.rs:41-69; x86 45-72 |
| Template handler lookup | materialized: cold-frame handler stack + unwind_throw; stack-owned: static active_catch_regions; finally/missing → Resume → bail | jit_exception_ops.rs:118-129,190-209; runtime_activation/exceptions.rs:62-98,103-135 |
| Template catch landing | route_throw_value → SideExit(catch_pc); Template never catches natively | reentry.rs:126-140; template/arm64.rs:1818-1819 |
| Machine catch | ExceptionalEdge::LandingPad; STUB_JIT_ACKNOWLEDGE_CAUGHT_THROW | machine/mod.rs:442-449; numeric/arm64.rs:352-369,4024-4040; reentry.rs:186-196 |
| Machine limits | declines finally, catch-less regions, async/generator | numeric/hir.rs:1016-1031 |
| Generators in Template | no Yield/Await TemplateOp → UnsupportedBail | template/plan.rs:15,1289 |
| Callee Throw at call site | classify, CODE_ENTRY_GENERATED_THROWS++, cleanup, restore roots, b throw_value | completion.rs:66-80,221-268; numeric/arm64.rs:4020-4040; template/x86_64/direct_call.rs:706-781 |
| Outer boundary | enter_compiled Throw → JitExecOutcome::Throw; Fatal → parked; OOM/Yield/Continue → Fatal; interp unwind_compiled_throw_above | entry/code.rs:186-199; interp/jit_call.rs:86-101,243-251; call_ops.rs:3990 |
| Fatal/termination | vm_error_to_throwable None → Fatal; cleanup_fatal forwards | error_ops.rs:395-397; reentry.rs:108-121; completion.rs:260-266 |
| Interrupt/Yield | backedge_poll Yield continue, Relink → SideExit, Err → Throw | reentry.rs:1583-1604; numeric/arm64.rs:270-281 |
| OOM | Probe domain only | runtime_stubs.rs:1279,1322; dispatch.rs:360-379 |
| Continue | exception op only | reentry.rs:516-522 |
| Stack overflow | generated code never raises RangeError: failed sp check / full Array → uncommitted_rejected/caller_transition (pre-effect side exit); interpreter retries → StackOverflow → RangeError | direct_call.rs:771-777,1079-1083; runtime_forward.rs:128-133; host.rs:30,397; error_ops.rs:391-394 |

### Per call-site cost (success)
- arm64 shared direct call: cmp/b.eq SideExit, cmp/b.eq Success (completion.rs:68-71), b result_ready, b cleanup (:159-161, :218), cmp/b.eq after cleanup (:246-247) ≈8 instrs, 3 taken branches, x1 live through cleanup.
- x86 Template 4 (template/x86_64/direct_call.rs:706-709) + rax/rdx spill/reload + test/je (:768-777); Machine x86 same (numeric/x86_64/direct_call.rs:745-753).
- Stubs: cbz x1 (transitions.rs:381); cmp/b.eq status word (:53-56).
- Callee: movz x1 on every return.
- Every try region ≥2 runtime calls (EnterTry, LeaveTry).

### V8-style needs
1. fp frames, ret addr → code, handler table per return offset; Template native-pc → bytecode-pc.
2. Unwinder repeating per-frame cleanup (completion.rs:221-268) — or remove those side effects first (publication redesign).
3. Rust stubs can't be unwound: return pending-exception sentinel, jump to shared unwind trampoline; check stays only at runtime transitions; enter_compiled = unwind floor (JSEntry).
4. Callee SideExit via status (completion.rs:162-217) → lazy deopt by ret-addr patching, or callee finishes itself in interpreter.
5. Template catch = leave to interpreter; unwinder synthesizes side exit at handler frame.
6. Fatal = uncatchable pending exception.

### Hazards
- pending_uncaught_frames via activation stack (jit_exception_ops.rs:195-198, jit_call.rs:93-95).
- Thrown value rooting during unwind (isolate root slot); GC in handler lookup.
- Machine catch needs acknowledgement stub.
- Stack overflow/activation limit rely on side exit + interpreter redo.
- Status word vs pair shapes; FINISH_ERROR + ROUTE_THROW, 4 targets change together.
- EnterTry mutates cold-frame state; JumpViaFinally can Return(value).

## Entry, JitCtx lifetime, interpreter↔JIT

| Topic | Fact | file:line |
|---|---|---|
| Entry points | maybe_dispatch_jit / dispatch_jit_sync_entry → run_optimized_frame / run_compiled_frame build VmRuntimeActivation{vm,stack,context,frame_index} (32 B by value) | interp/jit_call.rs:187-205, 1013-1030, 1098-1135, 1699-1720; jit.rs:2053-2062 |
| Entry gate | pc==0, no suspension owner; OSR too | jit_call.rs:1087-1091, 1106-1108, 438-441; frames.rs:97-100 |
| enter_compiled | NativeFrame, VmThread, JitCtx Rust-stack locals per entry; registers = interpreter arena window (no STACK_REGISTERS); thread.runtime_context=&activation | otter-jit/src/entry/code.rs:46-146 (window 63-70, frame 101-112, thread 116-131, ctx 133-146) |
| Cold state | initialize_native_frame_state; with_native_frame_extent writes back arguments_object | jit.rs:2146-2172, 2181-2215 |
| Publication at entry | Rust stub push, entry(&mut ctx), pop stub — pop BEFORE finish_compiled_entry | code.rs:150-168; entry/runtime_ops/calls.rs:29-57 |
| Array | Vec<JitNativeActivation{frame}> 8 B, len DEFAULT_MAX_STACK_DEPTH=1024, never resized | lib.rs:1294-1296; init.rs:280-284; run_control.rs:337; frames.rs:200-212 |
| Rust push | overflow StackOverflow → Fatal; arena frames also in jit_arena_activation_indices | frames.rs:172-198, 240-248; code.rs:150-153 |
| Generated limit | activation_limit=min(max_stack_depth,len); inline check → caller_transition | frames.rs:234-236; direct_call.rs:769-778; runtime_forward.rs:71-73; x86 :195-197 |
| Callee NativeFrame | native stack in caller linkage at sp; ≤4080 B | layout.rs:7-9, 77-99; direct_call.rs:110 |
| Per-call swap | push sp, top++, ctx.native_frame=sp, thread current_frame/code id; restore + zero slot | direct_call.rs:1438-1457; completion.rs:224-238 |
| Callee finds frame | only ctx.native_frame; Template sets x29, Machine not | template/arm64.rs:2028-2040; machine arm64.rs:5217-5224; x86_64.rs:2955 |
| register_stack | 512K-slot arena for interpreter windows only; lib.rs:1287 comment stale | register_stack.rs:28-30; frames.rs:110-112; lib.rs:1284-1290 |
| Outermost return | finish_compiled_entry_transaction: top!=0 ⇒ pending; else retire_unreferenced + reconcile | jit_call.rs:1311-1333, 1340-1370, 1379-1470 |
| Other top==0 users | jit_compile.rs:579, 833; exec.rs:57; jit_call.rs:2306-2318 | |
| OSR | optimizing pc=osr_pc + OSR_ENTRY, Machine dispatch clears; Template per-header trampolines pc=0 | code.rs:94-110; arm64.rs:1281-1296; template/code.rs:189-210; optimizing/mod.rs:260-283 |
| Kinds/flags | Interpreter 0, Baseline 1, Optimizing 2; HAS_SAFEPOINTS 1, STACK_REGISTERS 2, DERIVED 4, INCOMING_ARGS 8, OSR_ENTRY 16 | native_abi/frame.rs:95-136 |
| Generators/async | never enter compiled code; Yield(5) = budget poll only; generated frame never parked | call_ops.rs:3811-3820; template/arm64.rs:2199-2210; reentry.rs:1597; frames.rs:124-145 |
| Native stack limit | per enter_compiled: local marker − 512 KB | code.rs:89-92; host.rs:30, 397-399 |
| Sync reentry | cap 256 | host.rs:281-288; run_control.rs:340 |

### Nesting
Interp F → enter_compiled#1 → generated callees → stub/caller_transition → Rust → interp frame → enter_compiled#2. One JitCtx/VmThread/VmRuntimeActivation/entry NativeFrame per level on Rust stack; generated callees share level's JitCtx (abi.rs:33-34); one Array + top across levels; segments implicit (frame w/o STACK_REGISTERS, jit_arena_activation_indices). jit_generated_call_depth = top − arena_count − materialized (frames.rs:258-268) → logical_call_depth per bytecode call (host.rs:356-360).

### Needs
1. Entry record (prev exit-fp link, frame_index, entry NativeFrame).
2. Exit record: thread last-exit fp/pc at stub calls (replaces current_frame swapping).
3. fp chain for all generated frames; NativeFrame at fixed fp offset.
4. Thread entry-depth counter / entry-chain head (replaces top!=0 + retirement pin).
5. JS-depth counter or byte-only limit.

### Hazards
- Inline-parent publication pushes heap Vec<NativeFrame> into Array (inline_activations.rs:83-95; Drop 40-60).
- generated_caller_matches Array adjacency (generated.rs:75-112).
- GC + root-chain bound use Array (frames.rs:274-297).
- Retirement pinned by published frames (jit_call.rs:1322-1325; jit_compile.rs:576-581).
- Snapshot/deopt mirroring keyed by frame address (host.rs:367-380; native_stack_snapshot.rs:35-41); ordering from Array.
- Per-entry 512 KB limit relative to each nested entry marker.
- Side-exit check at entry vs outer pc (code.rs:176-183); with_native_frame_extent re-resolves ActivationStack (jit.rs:2198-2203).
- Rust push/pop out-of-line (code.rs:150,156).
- Test fixtures fabricate JitCtx/Array: template.rs:276-300; vm_ops.rs:335-350, 395-410; reentry.rs:1630-1645; machine/numeric/mod.rs:7991, 9974; runtime_stubs.rs:2778.
- Direct-call plan doesn't check generator/async (jit_registry.rs:399-428).

## Tier-up, code registry, entry cells

| Item | Location | Notes |
|---|---|---|
| `CodeEntryCell` (88 B) | `otter-vm/src/native_abi/code_entry.rs:94-136`, offsets `:285-300` | entry_addr@0 (atomic, 0=unlinked), code_object_id@8, flags@16, stack_frame_bytes@20, active_count@24, native_frame_header@32, break_even@48, tiering_enabled@56, entries@64, deopts@72, throws@80 |
| break-even at install | `jit_registry.rs:216-237` | TierCostModel::minimum_profitable_executions |
| `tiering_enabled` | `code_entry.rs:182-184` | 0 for Optimizing; `suppress_generated_tiering` (`jit_registry.rs:386-390`) |
| `CodeRegistryView{context, resolve_safepoint, hot_function@16}` | `native_abi/safepoints.rs:34-47` | mailbox fid+1 |
| `VmThread.current_code_object_id`/`code_registry` | `native_abi/frame.rs:35-41` | readers `runtime_stubs.rs:389`, `entry/runtime_ops/inline_frames.rs:33` |
| Code id alloc | `interp/jit_compile.rs:540,575` (T), `:783,829` (M) | monotonic `jit_next_code_object_id` (`lib.rs:1285`), never reused (`jit_registry.rs:263-267`) |
| Registry storage | `jit_registry.rs:101-122` | `codes: FxHashMap<id,RegisteredCode>`, `entry_cells` (tombstones), `function_entry_cells: FxHashMap<fid, Box<FunctionEntryCell>>` |
| Publication | `refresh_function_entry` `jit_registry.rs:752-787` | highest (Optimizing,id) Installed non-osr_only; pointer swap in permanent FunctionEntryCell; callers bake function-cell address (`arm64/direct_call.rs:1040-1068`) |
| `promote_hot_generated_callee` | `interp/jit_call.rs:1143-1154` | drained from `jit_backedge_poll` (`interp/stats.rs:212`), `run_optimized_frame` (`jit_call.rs:1104`), `resolve_jit_code_for_fid` (`:1224`) |
| `generated_feedback_clean` (JitCtx 0x40) | `entry/abi.rs:61,240`; read `entry/code.rs:168` | dirty ⇒ `finish_compiled_entry_transaction` (`jit_call.rs:1311-1333`) → `reconcile_generated_feedback` (`:1379-1470`) at outermost return |
| Interpreter counters | `note_jit_function_entry` `jit_call.rs:1476-1488`; back-edge `note_backedge_and_maybe_osr` `:265-360` (hash (fid, header_pc)) | generated entries summed `jit_compile.rs:189-190` |
| Template→Machine OSR | Template back-edge fuel poll `template/arm64.rs:2175-2212` → `backedge_poll` `runtime_activation/control.rs:41-65` → `baseline_backedges_reach_osr` `jit_call.rs:370-410` returns Relink (SideExit) | interpreter's next back-edge enters optimized OSR via `maybe_osr` (`:428-`) |
| Machine back-edge | `machine/numeric/arm64.rs:247-282` | `subs w29,#1` countdown (batch 16, `lib.rs:62`) then shared fuel cell (`VmThread.backedge_fuel_cell`, batch 4096, `interp/init.rs:1397`) |

### Caller-side counting sites
| Site | Tier | Fast path |
|---|---|---|
| `arm64/direct_call.rs:1468` | T+M arm64 | 6 below break-even (`tiering.rs:19-37`, `direct_call.rs:321-328`) + `str xzr, feedback_clean` (`:1471`); past break-even 8/12/15 |
| `arm64/direct_call/runtime_forward.rs:253,256` | T+M arm64 | same |
| `template/x86_64/direct_call.rs:699,702`, `:1008,1011` | T x86 | 5 (`x86_64_tiering.rs:28-47`) + `mov [r15+0x40],0` |
| `machine/numeric/x86_64/direct_call.rs:738,741`, `:1201,1204` | M x86 | same |
| `x86_64/runtime_forward.rs:418,421` | x86 | same |
| cold throws/deopts counters | all | `arm64/direct_call/completion.rs:79,170`; x86 template `:721,728,1024,1031`, machine `:822,829,1217,1224`, runtime_forward `:458,465` |

Publication beside: caller code id saved, `stp current_frame, code_object_id` (`arm64/direct_call.rs:1440-1456`, x86 `runtime_forward.rs:399-414`) ≈4 mem ops. Machine callers pay counter even though Optimizing callees never promote.

### Callee-side budget needs
1. Stable budget cell addressable by callee. `CodeEntryCell` boxed at install after emission (`jit_registry.rs:216`). Options: pre-allocate at id reservation (`jit_compile.rs:540`); patch relocation after install; budget in per-function `FunctionEntryCell` (permanent address, created at first install `jit_registry.rs:291-300`); or load via SELF.
2. Prologue + back-edge decrement; prologue cold stub runs promotion. Mid-activation compile allowed from poll (`stats.rs:212`); retirement gated on `jit_native_activation_top == 0` (`jit_compile.rs:579`).
3. Unify double counting (`jit_call.rs:1222`, `jit_compile.rs:189`).
4. Drop mailbox, `feedback_clean`, entries-derived stats (`returns = entries - deopts - throws`, `code_entry.rs:239-245`).
5. Optimizing code omits budget.

### Registry by return address
- None today; module invariant forbids address→generation index for target selection (`jit_registry.rs:35-37`). No base/len accessor on `JitFunctionCode` (`vm/src/jit.rs:2311,2335`); metadata has entry_offset + code_size (`native_abi/metadata.rs:36-39`; `template/code.rs:76-77`, `optimizing/mod.rs:113-114`).
- Each code object owns its own dynasmrt `ExecutableBuffer` (`otter-jit/src/code.rs:21-24`) — disjoint ranges; sorted Vec/BTreeMap enough.
- Add in `register_inner` (`jit_registry.rs:278-289`); remove in `retire_unreferenced` (`:598-609`).
- Return address → safepoint via vestigial `SafepointEntry.native_return_offset` (`safepoints.rs:152-156`).

### Hazards
- Stale code on stack: replacement never touches running frames; invalidation only unlinks entry_addr (`jit_registry.rs:792-809`). No lazy deopt/return-address patching. Inline protector cells (`frame.rs:50-72`).
- Invalidation producers: Protector, ShapeEpoch (`object_internal_ops/descriptors.rs:1029,1043`); chunk reclaim `invalidate_all` (`interp/exec.rs:97`).
- Code freed only at activation-top-0 epoch (`jit_call.rs:1318-1325`).
- Machine frame at poll has no safepoint roots → poll forbids collection (`control.rs:49-50`).
- **Machine arm64 uses x29 as poll countdown** (`machine/numeric/arm64.rs:57,250,1220`) though prologue saves x29/x30 (`:5223`) — conflicts with fp chain.
- OSR bodies (`osr_only`) excluded from `refresh_function_entry` (`jit_registry.rs:759`) but need range entries.
- `function_id_for_entry_addr` linear scan (`jit_registry.rs:375-382`), cold path.

## Completeness critic: items the area maps missed (paths under crates/)

**Generated code reading the frame beyond the prologue**
1. `otter-jit/src/machine/numeric/arm64.rs:5261-5287` (called `:3576`), x86 `machine/numeric/x86_64/direct_call.rs:155`: `emit_publish_forwarded_formals_context` writes a context value into the Machine caller's own window before a Forward call; leaf copy + arguments materialization read it. The Machine window is NOT deopt-only.
2. `machine/numeric/arm64.rs:2132` + `arm64/arguments.rs:24-45` (Template `template/arm64/scalar.rs:189`; x86 `x86_64/arguments.rs:24-42`, `machine/numeric/x86_64.rs:842`): arguments fast path reads arguments_object, INCOMING_ARGUMENTS, argument_count, register_count, register_base from the live frame.
3. `machine/numeric/arm64.rs:1423-1430` / `x86_64.rs:353-358`: TryBindDerivedThis reads STACK_REGISTERS|DERIVED_CONSTRUCTOR in generated code.
4. `machine/numeric/arm64.rs:2105` + `arm64/allocation.rs:223,251` (x86 `x86_64.rs:819`, `x86_64/allocation.rs:201,228`): inline arrow-closure allocation reads this/new_target from the frame; `arm64.rs:1354` EntryCallee reads SELF.
5. `arm64/direct_call/forward_bindings.rs:49-103`: forward calls reserve stack dynamically (`sub sp,#256`, size slot, caller base = sp+frame_bytes); read/write callee register_base + argument_count at sp. Dynamic sp conflicts with sp-relative root/raw offsets → fp-relative addressing.
6. `arm64/direct_call.rs:1392-1414` → `call_ops.rs:1105` `jit_copy_spread_arguments`: Rust writes into half-built unpublished callee frame; must stay no-GC.

**Safepoint producers**
7. Template safepoints: `otter-vm/src/interp/jit_compile.rs:1382`, `reserve_guarded_entry_safepoint` `:2690-2712` (FrameSlot whole window); ids via `method_ops/jit_snapshot.rs:67-240`, emitted `template/arm64/ic_probe.rs:1596-1602`.
8. `machine/numeric/inline_reentry.rs:80-135` produces `SafepointRecord.inline_frames` / `inline_frames_published` (recipe a return-address lookup would reuse).

**Tier and accounting**
9. `generated_entries_for_function`: `tier_policy.rs:455-463`, `jit_compile.rs:1618`.
10. Public stats on caller-side entries/deopts/throws: `otter-runtime/src/lib.rs:1005-1029, 4825, 4902` (fed by `jit_registry.rs:630-660`, also `active_count`); `otter-benchmark/src/bin/engine.rs:427-470` (`jit-generated-*`, tests `:3051-3076`); `scripts/cost.sh:111`; ~34 otter-runtime tests assert exact counts (e.g. `jit_stack_owned_array_construct.rs:325-337,713-812`, `jit_forward_arguments.rs:357`).
11. `interp/dispatch.rs:197`: `max_stack_depth_observed` adds `jit_generated_call_depth()`.
12. `runtime_activation/inline_activations.rs:87-93`: inline publication raises StackOverflow (RangeError) inside a stub; `otter-runtime/tests/jit_constant_owner.rs:199-229` pins generated-recursion overflow at `max_stack_depth(8)`.

**State constructors, layout asserts, goldens**
13. `interp/restore.rs:150-206`: second Interpreter constructor (snapshot restore) re-creates array, top, machine_roots, arena indices, materialized list.
14. trybuild goldens `otter-vm/tests/compile_fail/native_ctx_is_not_send.stderr:331-360`, `otter-runtime/tests/compile_fail/tokio_spawn_native_ctx_is_not_send.stderr:202` spell `*mut NativeFrame` → `JitNativeActivation` → `Vec` → `Interpreter`.
15. Layout asserts: `native_abi/frame.rs:374-386`, `jit.rs:1422-1431`, `code_entry.rs:287-301`, `entry/abi.rs:305` (JitCtx=104), `native_abi/runtime_stubs.rs:2106-2108`.

**Artifacts, docs, fixtures**
16. `artifact.rs:201-216` DirectCallArtifact (callee_native_frame_bytes, linkage_bytes, reserved_stack_bytes, callee_register_count) hashed `artifact/relocation.rs:1260-1276`, printed `artifact/assembly.rs:315-335`, asserted `otter-runtime/tests/jit_forward_arguments.rs:346`. `artifact.rs:663-684` hard-codes `nativeReturnOffset: null`, documented `docs/site/.../jit-debugging.md:933`, `AGENTS.md:557`; `otter-cli/tests/execution_config.rs:418-422` parses safepoints.json.
17. `JitFunctionCode` trait implementors: `otter-jit/src/measurement.rs:353-395` (ObservedJitCode), test fake `jit_registry.rs:~898-933`.
18. Fixtures fabricating frames: `arm64/direct_call/tiering.rs:41-80`, `entry/x86_64_tiering.rs:50-90`, `entry/runtime_ops/inline_frames.rs:104-160`, `runtime_activation/inline_activations_tests.rs:135-292`, `runtime_activation/value_ops.rs:835-1483` (`:1352`), `frame_ops.rs:164-181`, `jit_runtime_ops.rs:342-400`, `native_abi/code_entry.rs:315-341`.
19. `scripts/dev/jitev.py:17-19`, `jitevents.py:66` parse `generatedCallDeopt` payload.

Nothing found: no FFI JSCallback in repo; native_leaf/inline_leaf publish nothing; relocation kinds encode no JitCtx/frame offsets.

**Corrections**: `interp/restore.rs` IS a producer of every activation field (deopt map said unrelated). Stub-raised StackOverflow (item 12) exists. Background contract doc: JitCtx is 104 B (not 120); Machine window not deopt-only (item 1); Machine prologue at `arm64.rs:5216-5253`. `docs/site/.../engine/arguments.md:38-39` claims a 72-byte NativeFrame with arguments_object at 68 — stale (56 B, 0x34).

