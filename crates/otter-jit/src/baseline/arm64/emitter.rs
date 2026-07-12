//! Linear AArch64 baseline emission coordinator.
//!
//! # Contents
//! - BaselinePlan consumption and label allocation.
//! - One-pass bytecode opcode dispatch into focused emitter modules.
//! - Bail/throw epilogues, OSR trampolines, and code finalization.
//!
//! # Invariants
//! - Planning and validation complete before the assembler is opened.
//! - Every instruction stamps its canonical resume PC before observable work.
//! - Block entries invalidate advisory machine-register residency.
//! - Finalized code and embedded pointers transfer to one BaselineCode owner.

use super::*;

/// Mutable state owned for exactly one baseline compilation.
struct EmissionSession {
    ops: Assembler,
    bail: DynamicLabel,
    threw: DynamicLabel,
    labels: BTreeMap<u32, DynamicLabel>,
    artifacts: EmissionArtifacts,
    fres: FloatResidency,
    osr_only: bool,
}

impl EmissionSession {
    fn new(plan: &BaselinePlan) -> Self {
        let mut ops = Assembler::new().expect("assembler alloc");
        let bail = ops.new_dynamic_label();
        let threw = ops.new_dynamic_label();
        let labels = plan
            .instructions
            .iter()
            .map(|instruction| (instruction.instruction_pc, ops.new_dynamic_label()))
            .collect();
        Self {
            ops,
            bail,
            threw,
            labels,
            artifacts: EmissionArtifacts::new(plan.load_property_count, plan.store_property_count),
            fres: FloatResidency::default(),
            osr_only: false,
        }
    }
}

pub(in crate::baseline) fn compile(view: &JitCompileSnapshot) -> Result<BaselineCode, Unsupported> {
    let plan = BaselinePlan::build(view)?;
    EmissionSession::new(&plan).emit(view, plan)
}

impl EmissionSession {
    fn emit(
        mut self,
        view: &JitCompileSnapshot,
        mut plan: BaselinePlan,
    ) -> Result<BaselineCode, Unsupported> {
        let code_block = view.code_block.as_ref();

        let ops = &mut self.ops;
        let bail = self.bail;
        let threw = self.threw;
        let labels = &self.labels;
        let artifacts = &mut self.artifacts;
        let fres = &mut self.fres;
        let osr_only = &mut self.osr_only;

        // Loop headers are verified once by the owning CodeBlock.
        let loop_headers = code_block.loop_headers();

        // Incoming register state is unknown at every verified block entry.
        let block_starts = code_block.block_starts();

        // FP-residency read cache (OPTIMIZING_TIER.md S1) is enabled only for
        // float-natured functions — those that divide. Integer-heavy code (no
        // `Op::Div`) keeps the byte-identical int-fast-path emit, so this can
        // never slow a non-dividing function. `Op::Div` always produces a
        // Number via `f64`, so a function that contains one already runs its
        // arithmetic through the double path on the hot values.
        let enable_fres = plan.enable_float_residency;
        let entry = ops.offset();
        // Self-recursion target: a direct `Op::Call` to the running closure
        // re-enters here (a fresh callee `JitCtx` in `x0`) without a Rust
        // frame-build bridge. Only used when the body is frame-index-free.
        let self_call_safe = is_self_call_safe(view);
        let self_entry = ops.new_dynamic_label();
        dynasm!(ops ; .arch aarch64 ; =>self_entry);
        emit_prologue(ops);

        // Stable GC cage base, baked for inline property-load decompression.
        let cage_base = view.cage_base;
        for (instr, lowered) in view.instructions.iter().zip(&plan.instructions) {
            let instruction_pc = lowered.instruction_pc;
            dynasm!(ops ; .arch aarch64 ; =>labels[&instruction_pc]);
            // A branch target is a block boundary: control can arrive here from
            // elsewhere with unknown register state (and OSR enters loop headers
            // with values freshly loaded from memory), so no FP register can be
            // assumed to hold a slot's value.
            if enable_fres && block_starts.binary_search(&instruction_pc).is_ok() {
                fres.clear();
            }
            // Publish this op's logical PC before any guard, mutation, call, or
            // allocation. The NativeFrame copy is authoritative to GC,
            // diagnostics, and top-level dispatch. The context copy is the
            // machine-local payload retained by nested compiled calls while
            // their parent NativeFrame is restored by the call tail.
            emit_load_u64(ops, 9, u64::from(instruction_pc));
            dynasm!(ops
                ; .arch aarch64
                ; str w9, [x20, RESUME_PC_OFFSET]
                ; ldr x10, [x20, NATIVE_FRAME_OFFSET]
                ; str w9, [x10, NATIVE_FRAME_PC_OFFSET]
            );
            match lowered.op {
                Op::LoadInt32 => {
                    let operands = lowered.load_int32_operands()?;
                    let boxed = value_tag::NUMBER_TAG | u64::from(operands.value as u32);
                    emit_load_u64(ops, 9, boxed);
                    store_reg(ops, 9, operands.dst)?;
                }
                Op::LoadNumber => {
                    let dst = lowered.destination_operands()?.dst;
                    let Some(value) = instr.load_number else {
                        return Err(Unsupported::OperandShape("load-number constant"));
                    };
                    // Materialize the boxed `Value` (int32 or offset-double) inline
                    // instead of re-running the constant load through the delegate
                    // bridge: a float literal in a numeric loop otherwise pays a VM
                    // round-trip on every execution.
                    emit_load_u64(ops, 9, otter_vm::Value::number_f64(value).to_bits());
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadLocal => {
                    let operands = lowered.local_operands()?;
                    load_reg(ops, 9, operands.local)?;
                    store_reg(ops, 9, operands.value)?;
                }
                Op::LoadUndefined => {
                    let dst = lowered.destination_operands()?.dst;
                    // SPECIAL payload 0 == undefined.
                    emit_load_u64(ops, 9, VALUE_UNDEFINED);
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadNull => {
                    let dst = lowered.destination_operands()?.dst;
                    emit_load_u64(ops, 9, VALUE_NULL);
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadHole => {
                    let dst = lowered.destination_operands()?.dst;
                    // SPECIAL payload `SPECIAL_HOLE` == the TDZ/uninitialized hole.
                    emit_load_u64(ops, 9, VALUE_HOLE);
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadTrue => {
                    let dst = lowered.destination_operands()?.dst;
                    emit_load_u64(ops, 9, VALUE_TRUE);
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadFalse => {
                    let dst = lowered.destination_operands()?.dst;
                    emit_load_u64(ops, 9, VALUE_FALSE);
                    store_reg(ops, 9, dst)?;
                }
                Op::StoreLocal => {
                    let operands = lowered.local_operands()?;
                    load_reg(ops, 9, operands.value)?;
                    store_reg(ops, 9, operands.local)?;
                }
                Op::Add => emit_add_with_runtime_fallback(
                    ops,
                    lowered.binary_operands()?,
                    plan.add_alloc_safepoints.get(&lowered.byte_pc).copied(),
                    view.code_block.register_count,
                    threw,
                )?,
                Op::Sub | Op::Mul | Op::Div if enable_fres => {
                    emit_float_binop_res(ops, lowered.binary_operands()?, bail, lowered.op, fres)?;
                }
                Op::Sub | Op::Mul => {
                    emit_add_sub_mul(ops, lowered.binary_operands()?, bail, lowered.op)?;
                }
                Op::Div => emit_div(ops, lowered.binary_operands()?, bail)?,
                Op::Rem => emit_rem(ops, lowered.binary_operands()?, bail)?,
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
                    if enable_fres =>
                {
                    let cmp = match lowered.op {
                        Op::LessThan => Cmp::Lt,
                        Op::LessEq => Cmp::Le,
                        Op::GreaterThan => Cmp::Gt,
                        Op::GreaterEq => Cmp::Ge,
                        Op::Equal => Cmp::Eq,
                        _ => Cmp::Ne,
                    };
                    emit_cmp_res(ops, lowered.binary_operands()?, bail, cmp, fres)?;
                }
                Op::LessThan => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Lt)?,
                Op::LessEq => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Le)?,
                Op::GreaterThan => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Gt)?,
                Op::GreaterEq => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Ge)?,
                Op::Equal => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Eq)?,
                Op::NotEqual => emit_cmp(ops, lowered.binary_operands()?, bail, Cmp::Ne)?,
                // `ToPrimitive` is identity on primitives. Object/function
                // families bail so observable coercion hooks run in the VM.
                Op::ToPrimitive => {
                    let operands = lowered.unary_operands()?;
                    emit_to_primitive_identity(ops, operands.dst, operands.src, bail)?;
                }
                // `ToNumeric` is identity on a number (int32 or double); emit
                // a guarded move. Other primitives/objects need the VM path.
                Op::ToNumeric => {
                    let operands = lowered.unary_operands()?;
                    load_reg(ops, 9, operands.src)?;
                    guard_number!(ops, 9, bail);
                    store_reg(ops, 9, operands.dst)?;
                }
                Op::Jump => {
                    let target = lowered.branch_operands()?.target;
                    let tgt = labels[&target];
                    if target <= instruction_pc {
                        emit_backedge_interrupt_check(ops, threw);
                    }
                    dynasm!(ops ; .arch aarch64 ; b =>tgt);
                }
                Op::JumpIfFalse | Op::JumpIfTrue => {
                    let operands = lowered.conditional_branch_operands()?;
                    let cond = operands.condition;
                    let target = operands.target;
                    let tgt = labels[&target];
                    load_reg(ops, 9, cond)?;
                    // Only boolean conditions are supported in this subset.
                    dynasm!(ops
                        ; .arch aarch64
                        ; sub x14, x9, #(VALUE_FALSE as u32)          // bail unless boolean
                        ; cmp x14, #1
                        ; b.hi =>bail
                        ; cmp x9, #(VALUE_TRUE as u32)                // eq iff true
                    );
                    if target <= instruction_pc {
                        let taken = ops.new_dynamic_label();
                        let fallthrough = ops.new_dynamic_label();
                        if matches!(lowered.op, Op::JumpIfFalse) {
                            dynasm!(ops ; .arch aarch64 ; b.ne =>taken);
                        } else {
                            dynasm!(ops ; .arch aarch64 ; b.eq =>taken);
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>fallthrough ; =>taken);
                        emit_backedge_interrupt_check(ops, threw);
                        dynasm!(ops ; .arch aarch64 ; b =>tgt ; =>fallthrough);
                    } else if matches!(lowered.op, Op::JumpIfFalse) {
                        dynasm!(ops ; .arch aarch64 ; b.ne =>tgt);
                    } else {
                        dynasm!(ops ; .arch aarch64 ; b.eq =>tgt);
                    }
                }
                Op::MakeFunction | Op::MakeClosure if instr.make_self => {
                    // SELF binding: the closure value is precomputed in
                    // `JitCtx.self_closure` (offset 8 from x20), so read it
                    // straight into `dst` — no Rust round-trip through
                    // the function/closure builder.
                    let dst = lowered.destination_operands()?.dst;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, #8]);
                    store_reg(ops, 9, dst)?;
                }
                Op::MakeFunction => {
                    let operands = lowered.constant_operands()?;
                    let dst = operands.dst;
                    let idx = operands.constant;
                    // jit_make_fn_stub(ctx=x20, dst, idx) -> status in x0.
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(ops, 2, u64::from(idx));
                    emit_call_stub(ops, jit_make_fn_stub as *const () as usize, threw);
                }
                Op::NewObject => {
                    let dst = lowered.destination_operands()?.dst;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_call_stub(ops, jit_new_object_stub as *const () as usize, threw);
                }
                Op::NewArray => {
                    let operands = lowered.new_array_operands()?;
                    let dst = operands.dst;
                    let source_regs = plan.register_tail(operands.elements)?;
                    let count = source_regs.len();
                    let source_regs_ptr = source_regs.as_ptr();
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(ops, 2, source_regs_ptr as u64);
                    emit_load_u64(ops, 3, count as u64);
                    emit_call_stub(ops, jit_new_array_stub as *const () as usize, threw);
                }
                Op::Call => {
                    let call = lowered.call_operands()?;
                    let call_operands = CallOperandView::Plain {
                        call,
                        arguments: plan.register_tail(call.arguments)?,
                    };
                    // Splice a tiny monomorphic leaf callee inline under an
                    // identity guard (no per-call bridge); fall back to the
                    // direct-call bridge for absent / ineligible sites.
                    let inlined = match view.inline_callees.get(&lowered.byte_pc) {
                        Some(callee) => {
                            try_emit_inline_call(ops, callee, call_operands, cage_base, bail)?
                        }
                        None => false,
                    };
                    if !inlined {
                        // A frame-index-free function re-enters self-recursive
                        // calls inline (no Rust frame-build bridge), bailing on a
                        // guard miss; any other function takes the direct-call
                        // bridge.
                        if self_call_safe {
                            emit_self_recursive_call(
                                ops,
                                call_operands,
                                view.code_block.register_count,
                                self_entry,
                                bail,
                                threw,
                            )?;
                        } else {
                            emit_call(ops, call_operands, bail, threw)?;
                        }
                    }
                }
                // `recv.name(args…)` — IC-resolve the method + direct-branch to
                // its compiled entry (WhiskerIC method call), falling back to the
                // in-place full method-call stub when ineligible.
                Op::CallMethodValue => {
                    let call = lowered.method_call_operands()?;
                    let call_operands = CallOperandView::Method {
                        call,
                        arguments: plan.register_tail(call.arguments)?,
                    };
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;
                    // Splice a tiny monomorphic read-only method inline under an
                    // identity + receiver-shape guard; fall back to the method
                    // bridge for absent / ineligible sites.
                    let inlined = match view.inline_methods.get(&lowered.byte_pc) {
                        Some(method) => try_emit_inline_method_call(
                            ops,
                            method,
                            call_operands,
                            site,
                            cage_base,
                            view.object_shape_byte,
                            view.object_values_ptr_byte,
                            view.jit_proto_byte,
                            view.closure_fid_byte,
                            bail,
                            threw,
                        )?,
                        // A polymorphic site (no single monomorphic entry) emits a
                        // most-frequent-first guard chain over its observed
                        // receiver shapes, bridging only when none match.
                        None => match view.inline_poly_methods.get(&lowered.byte_pc) {
                            Some(methods) => try_emit_poly_inline_method_call(
                                ops,
                                methods,
                                call_operands,
                                site,
                                cage_base,
                                view.object_shape_byte,
                                view.object_values_ptr_byte,
                                view.jit_proto_byte,
                                view.closure_fid_byte,
                                bail,
                                threw,
                            )?,
                            None => false,
                        },
                    };
                    if !inlined {
                        emit_method_call(
                            ops,
                            call_operands,
                            site,
                            view.collection_leaf_methods.get(&lowered.byte_pc),
                            view.collection_alloc_methods.get(&lowered.byte_pc),
                            Some(view),
                            plan.method_alloc_safepoints.get(&lowered.byte_pc).copied(),
                            bail,
                            threw,
                        )?;
                    }
                }
                // Element backing stores are VM-owned containers, not native
                // ABI. Delegate the complete operation from the published
                // frame instead of probing Rust `Vec` layout.
                Op::LoadElement => {
                    let operands = lowered.element_load_operands()?;
                    let dst = operands.dst;
                    let recv = operands.receiver;
                    let idx = operands.index;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, recv as u32
                        ; movz x3, idx as u32
                    );
                    emit_call_stub(ops, jit_load_element_stub as *const () as usize, threw);
                }
                Op::StoreElement => {
                    let operands = lowered.element_store_operands()?;
                    let recv = operands.receiver;
                    let idx = operands.index;
                    let src = operands.value;
                    let scratch = operands.scratch;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, recv as u32
                        ; movz x2, idx as u32
                        ; movz x3, src as u32
                        ; movz x4, scratch as u32
                    );
                    emit_call_stub(ops, jit_store_element_stub as *const () as usize, threw);
                }
                // `dst = global[name]` or throw — delegate to the safe bridge.
                Op::LoadGlobalOrThrow => {
                    let operands = lowered.constant_operands()?;
                    let dst = operands.dst;
                    let name = operands.constant;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(ops, 2, u64::from(name));
                    emit_load_u64(ops, 3, u64::from(view.code_block.id));
                    emit_call_stub(ops, jit_load_global_stub as *const () as usize, threw);
                }
                // `dst = upvalue[idx]` (captured binding). Inline: read the cell
                // handle from the frame's upvalue spine, decompress (cells are
                // old-space, immobile), load the captured Value. A TDZ hole or a
                // `0` spine base (no upvalues / direct-call ctx) misses to the
                // bridge stub, which raises the `ReferenceError`. `idx` is the
                // signed bytecode index, passed as u32 bits and re-read as i32.
                Op::LoadUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    let dst = operands.value;
                    let idx = operands.index;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let dst_off = reg_offset(dst)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; ldr x9, [x13, UPVALUE_VALUE_OFFSET] // captured Value
                        );
                        emit_load_u64(ops, 11, VALUE_HOLE);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x9, x11                        // TDZ hole?
                            ; b.eq =>up_miss
                            ; str x9, [x19, dst_off]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops ; .arch aarch64 ; =>up_miss ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(ops, 2, u64::from(idx as u32));
                    emit_call_stub(ops, jit_load_upvalue_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
                }
                // `upvalue[idx] = src` (captured binding). Inline the primitive
                // store: a non-pointer value written into the (old-space) cell
                // needs no write barrier. A pointer value or `0` spine base
                // misses to the bridge stub, which performs the barriered store.
                Op::StoreUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    let src = operands.value;
                    let idx = operands.index;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let src_off = reg_offset(src)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr x12, [x19, src_off]            // value to store
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x12, x11
                            ; b.eq =>up_miss                     // pointer -> barriered stub
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; str x12, [x13, UPVALUE_VALUE_OFFSET]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops ; .arch aarch64 ; =>up_miss ; mov x0, x20 ; movz x1, src as u32);
                    emit_load_u64(ops, 2, u64::from(idx as u32));
                    emit_call_stub(ops, jit_store_upvalue_stub as *const () as usize, threw);
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
                }
                // `upvalue[idx] = src` with a TDZ guard (assignment to a captured
                // `let`/`const`). Like `StoreUpvalue` but reads the cell first and
                // misses to the delegate bridge on a hole (raising the
                // `ReferenceError`). Inlines only the primitive store; a pointer
                // value misses to the bridge (barriered store inside).
                Op::StoreUpvalueChecked => {
                    let operands = lowered.upvalue_operands()?;
                    let src = operands.value;
                    let idx = operands.index;
                    let up_miss = ops.new_dynamic_label();
                    let up_done = ops.new_dynamic_label();

                    if cage_base != 0 && idx >= 0 {
                        let src_off = reg_offset(src)?;
                        let idx_off = (idx as u32) * UPVALUE_CELL_SIZE;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x20, UPVALUES_PTR_OFFSET] // spine base
                            ; cbz x9, =>up_miss
                            ; ldr x12, [x19, src_off]            // value to store
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x12, x11
                            ; b.eq =>up_miss                     // pointer -> barriered bridge
                            ; ldr w10, [x9, idx_off]             // 4-byte cell handle
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        emit_load_u64(ops, 11, VALUE_HOLE);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x10                  // cell body ptr
                            ; ldr x14, [x13, UPVALUE_VALUE_OFFSET] // current value
                            ; cmp x14, x11                       // TDZ hole?
                            ; b.eq =>up_miss
                            ; str x12, [x13, UPVALUE_VALUE_OFFSET]
                            ; b =>up_done
                        );
                    }

                    dynasm!(ops
                        ; .arch aarch64
                        ; =>up_miss
                        ; mov x0, x20
                        ; movz x1, src as u32
                    );
                    emit_load_u64(ops, 2, u64::from(idx as u32));
                    emit_call_stub(
                        ops,
                        jit_store_upvalue_checked_stub as *const () as usize,
                        threw,
                    );
                    dynasm!(ops ; .arch aarch64 ; =>up_done);
                }
                // `dst = ToNumeric(src) + delta` (§13.4 UpdateExpression). Int32
                // fast path with overflow → double; double path otherwise.
                Op::Increment => {
                    let operands = lowered.increment_operands()?;
                    let dst = operands.dst;
                    let src = operands.src;
                    let delta = operands.delta;
                    load_reg(ops, 9, src)?;
                    emit_load_u64(ops, 12, u64::from(delta as u32));
                    let float_path = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();
                    dynasm!(ops
                        ; .arch aarch64
                        ; movz x15, NUMBER_TAG_HI16, lsl #48
                        ; and x14, x9, x15
                        ; cmp x14, x15
                        ; b.ne =>float_path
                        ; adds w13, w9, w12
                        ; b.vs =>float_path
                    );
                    box_int32!(ops, 13, 11);
                    store_reg(ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; b =>done ; =>float_path);
                    emit_num_to_double(ops, 9, 0, bail);
                    dynasm!(ops ; .arch aarch64 ; scvtf d1, w12 ; fadd d2, d0, d1);
                    emit_box_double(ops, 2, 13);
                    store_reg(ops, 13, dst)?;
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::LoadThis => {
                    // `this` bits are precomputed in `JitCtx.this_value`
                    // (offset 16 from x20). Bail on a hole — a derived-ctor
                    // `this`-before-super, which the interpreter resolves.
                    let dst = lowered.destination_operands()?.dst;
                    let hole = VALUE_HOLE;
                    dynasm!(ops ; .arch aarch64 ; ldr x9, [x20, THIS_VALUE_OFFSET]);
                    emit_load_u64(ops, 12, hole);
                    dynasm!(ops ; .arch aarch64 ; cmp x9, x12 ; b.eq =>bail);
                    store_reg(ops, 9, dst)?;
                }
                Op::LoadProperty => {
                    // jit_load_prop_window_stub(ctx=x20, dst, obj, name_idx, site, cell).
                    // `site` is the dense IC index from the snapshot, used by
                    // the bridge for the monomorphic fast path (PC-keyed lookup
                    // is unavailable at PC 0); `usize::MAX` means "no site".
                    // `cell` is this site's self-patching WhiskerIC cell.
                    let operands = lowered.property_load_operands()?;
                    let dst = operands.dst;
                    let obj = operands.object;
                    let name = operands.name;
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;

                    // This site's WhiskerIC cell address (stable for the code's
                    // life). Filled by the stub on a monomorphic own-data hit.
                    let cell_addr = artifacts.next_load_ic_addr();

                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();

                    if cage_base != 0 && instr.load_array_length {
                        let obj_off = reg_offset(obj)?;
                        let dst_off = reg_offset(dst)?;
                        let array_tag = u32::from(view.array_layout.type_tag);
                        let length_byte = view.array_layout.length_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]
                            ; cmp w14, array_tag
                            ; b.ne =>miss
                            ; ldr x9, [x13, length_byte]
                        );
                        emit_load_u64(ops, 12, i32::MAX as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cmp x9, x12
                            ; b.hi =>miss
                        );
                        box_int32!(ops, 9, 12);
                        dynasm!(ops
                            ; .arch aarch64
                            ; str x9, [x19, dst_off]
                            ; b =>done
                        );
                    }

                    // Inline guarded own-data load through the self-patching
                    // cell: guard tag + GC type tag + cell shape, then read the
                    // value slab slot at the cell's byte offset. No allocation /
                    // call → no safepoint; the object pointer is recomputed from
                    // the (rooted) frame slot each time, never held across one.
                    // Shape `0` is reserved as the empty-cell sentinel. Some
                    // live shapes can currently have offset 0, so those shapes
                    // deliberately miss to the stub until the cell grows an
                    // explicit valid bit.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let dst_off = reg_offset(dst)?;
                        let shape_byte = view.object_shape_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset (zero-ext)
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]          // header type tag
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte] // receiver shape handle
                            ; cbz w14, =>miss
                        );
                        emit_load_u64(ops, 15, cell_addr as u64);
                        // Walk the IC ways. The `cbz` above prevents empty ways
                        // (`shape == 0`) from matching a live shape-0 object.
                        // A hit loads that way's value byte into w17 and shares
                        // the slab read.
                        let do_load = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let vbyte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, vbyte_off]
                                ; b =>do_load
                                ; =>next
                            );
                        }
                        dynasm!(ops ; .arch aarch64 ; b =>miss ; =>do_load);
                        // Slab base from the fresh header (inline) or stable
                        // out-of-line `values_ptr` — never the cached body pointer.
                        emit_slab_base(ops, view, 13, 14);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>miss
                            ; ldr w9, [x13, x17]       // 4-byte compressed slot
                        );
                        emit_decompress_slot(ops, cage_base as u64, miss);
                        dynasm!(ops
                            ; .arch aarch64
                            ; str x9, [x19, dst_off]
                            ; b =>done
                        );
                    }

                    // Miss / no cage base: shared runtime IC + general path,
                    // passing the cell so the stub can self-patch it.
                    dynasm!(ops ; .arch aarch64 ; =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, dst as u32
                        ; movz x2, obj as u32
                    );
                    emit_load_u64(ops, 3, u64::from(name));
                    emit_load_u64(ops, 4, site);
                    emit_load_u64(ops, 5, cell_addr as u64);
                    emit_load_u64(ops, 6, u64::from(view.code_block.id));
                    // The typed window operation handles only own-data IC
                    // resolution and self-patching. Full `[[Get]]` semantics
                    // bail to normal dispatch instead of re-entering one
                    // interpreter opcode through a framed bridge.
                    emit_load_u64(ops, 16, jit_load_prop_window_stub as *const () as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cmp x0, #1
                        ; b.eq =>threw
                        ; cmp x0, #2
                        ; b.eq =>bail
                    );
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::StoreProperty => {
                    // Operands: obj, name_const, src, scratch_dst.
                    // jit_store_prop_window_stub(ctx=x20, obj, name_idx, src, site, cell).
                    let operands = lowered.property_store_operands()?;
                    let obj = operands.object;
                    let name = operands.name;
                    let src = operands.value;
                    let site = instr.property_ic_site(code_block).unwrap_or(usize::MAX) as u64;

                    let cell_addr = artifacts.next_store_ic_addr();

                    let miss = ops.new_dynamic_label();
                    let done = ops.new_dynamic_label();

                    // Inline guarded existing-own-data store through the
                    // self-patching cell: guard tag + GC type tag + cell shape,
                    // write the value into the value slab slot, then a
                    // value-tag-gated write barrier (primitive stores skip it).
                    // No allocation → no safepoint; the object pointer is
                    // recomputed from the (rooted) frame slot, never held
                    // across one. Shape-0 receiver / empty cell / guard miss →
                    // shared stub.
                    if cage_base != 0 {
                        let obj_off = reg_offset(obj)?;
                        let src_off = reg_offset(src)?;
                        let shape_byte = view.object_shape_byte;
                        dynasm!(ops
                            ; .arch aarch64
                            ; ldr x9, [x19, obj_off]   // receiver Value
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>miss
                            ; mov w12, w9              // low-32 Gc offset
                        );
                        emit_load_u64(ops, 13, cage_base as u64);
                        dynasm!(ops
                            ; .arch aarch64
                            ; add x13, x13, x12        // x13 = GcHeader ptr
                            ; ldrb w14, [x13]
                            ; cmp w14, OBJECT_BODY_TYPE_TAG
                            ; b.ne =>miss
                            ; ldr w14, [x13, shape_byte] // receiver shape handle
                            ; cbz w14, =>miss
                        );
                        emit_load_u64(ops, 15, cell_addr as u64);
                        // N-way IC walk (see `LoadProperty`): match a way's shape,
                        // load its value byte into w17, then share the slab write.
                        let do_store = ops.new_dynamic_label();
                        for way in 0..IC_WAYS as u32 {
                            let shape_off = way * 8;
                            let vbyte_off = shape_off + 4;
                            let next = ops.new_dynamic_label();
                            dynasm!(ops
                                ; .arch aarch64
                                ; ldr w16, [x15, shape_off]
                                ; cmp w14, w16
                                ; b.ne =>next
                                ; ldr w17, [x15, vbyte_off]
                                ; b =>do_store
                                ; =>next
                            );
                        }
                        let store_prim = ops.new_dynamic_label();
                        dynasm!(ops
                            ; .arch aarch64
                            ; b =>miss
                            ; =>do_store
                            ; ldr x9, [x19, src_off]   // value to store
                        );
                        // Slab base from the fresh header (inline) or stable
                        // out-of-line `values_ptr` — never the cached body pointer.
                        emit_slab_base(ops, view, 13, 14);
                        dynasm!(ops
                            ; .arch aarch64
                            ; cbz x13, =>miss
                            ; movz x11, NUMBER_TAG_HI16, lsl #48
                            ; orr x11, x11, #value_tag::OTHER_TAG  // NOT_CELL_MASK
                            ; tst x9, x11
                            ; b.ne =>store_prim        // primitive → compress, no barrier
                            // Cell: the compressed ref is the low-32 8-aligned
                            // offset (low-3 tag 000), i.e. the value's low word.
                            ; str w9, [x13, x17]
                        );
                        // Pointer value: card-mark the parent header. A
                        // frameless-eligible body uses the window barrier (reads
                        // the parent/child from the register window) so it is
                        // sound with no `HoltStack` frame.
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x20
                            ; movz x1, obj as u32
                            ; movz x2, src as u32
                        );
                        let barrier = if self_call_safe {
                            jit_write_barrier_window_stub as *const () as usize
                        } else {
                            jit_write_barrier_stub as *const () as usize
                        };
                        emit_call_stub(ops, barrier, threw);
                        dynasm!(ops ; .arch aarch64 ; b =>done ; =>store_prim);
                        // A wide int / double / function id cannot inline-compress
                        // (a boxed number allocates); the runtime store handles it.
                        emit_compress_slot_or_bail(ops, miss);
                        dynasm!(ops ; .arch aarch64 ; str w10, [x13, x17] ; b =>done);
                    }

                    // Miss / no cage base: shared runtime store path, passing
                    // the cell so the stub can self-patch it.
                    dynasm!(ops ; .arch aarch64 ; =>miss);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, obj as u32
                    );
                    emit_load_u64(ops, 2, u64::from(name));
                    dynasm!(ops ; .arch aarch64 ; movz x3, src as u32);
                    emit_load_u64(ops, 4, site);
                    emit_load_u64(ops, 5, cell_addr as u64);
                    emit_load_u64(ops, 6, u64::from(view.code_block.id));
                    emit_load_u64(ops, 16, jit_store_prop_window_stub as *const () as u64);
                    dynasm!(ops
                        ; .arch aarch64
                        ; blr x16
                        ; cmp x0, #1
                        ; b.eq =>threw
                        ; cmp x0, #2
                        ; b.eq =>bail
                    );
                    dynasm!(ops ; .arch aarch64 ; =>done);
                }
                Op::BitwiseOr => {
                    emit_int_binop(ops, lowered.binary_operands()?, bail, IntBinOp::Or)?
                }
                Op::BitwiseAnd => {
                    emit_int_binop(ops, lowered.binary_operands()?, bail, IntBinOp::And)?
                }
                Op::BitwiseXor => {
                    emit_int_binop(ops, lowered.binary_operands()?, bail, IntBinOp::Xor)?
                }
                Op::Shl => emit_int_binop(ops, lowered.binary_operands()?, bail, IntBinOp::Shl)?,
                Op::Shr => emit_int_binop(ops, lowered.binary_operands()?, bail, IntBinOp::Shr)?,
                Op::Ushr => emit_ushr(ops, lowered.binary_operands()?, bail)?,
                Op::Return | Op::ReturnValue => {
                    let src = lowered.source_operands()?.src;
                    let off = reg_offset(src)?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; ldr x0, [x19, off]
                        ; movz x1, STATUS_RETURNED as u32
                    );
                    emit_epilogue(ops);
                }
                Op::ReturnUndefined => {
                    let undef = VALUE_UNDEFINED; // SPECIAL_UNDEFINED == 0
                    emit_load_u64(ops, 0, undef);
                    dynasm!(ops ; .arch aarch64 ; movz x1, STATUS_RETURNED as u32);
                    emit_epilogue(ops);
                }
                // Variadic operations still using compile-owned decoded operand
                // metadata. Fixed-operand slow paths below use typed ABI stubs.
                Op::MathCall => {
                    let operands = lowered.math_call_operands()?;
                    let dst = operands.dst;
                    let method_id = operands.method;
                    let argument_regs = plan.register_tail(operands.arguments)?;
                    let argc = argument_regs.len();
                    if argc == 0
                        && otter_bytecode::method_id::MathMethod::from_u32(method_id)
                            == Some(otter_bytecode::method_id::MathMethod::Random)
                    {
                        emit_load_u64(ops, 16, otter_jit_math_random as *const () as u64);
                        dynasm!(ops ; .arch aarch64 ; blr x16);
                        store_reg(ops, 0, dst)?;
                    } else {
                        let argument_regs_ptr = argument_regs.as_ptr();
                        dynasm!(ops
                            ; .arch aarch64
                            ; mov x0, x20
                            ; movz x1, dst as u32
                        );
                        emit_load_u64(ops, 2, u64::from(method_id));
                        emit_load_u64(ops, 3, argument_regs_ptr as u64);
                        emit_load_u64(ops, 4, argc as u64);
                        emit_call_stub(ops, jit_math_call_stub as *const () as usize, threw);
                    }
                }
                Op::MakeClosure => {
                    let operands = lowered.make_closure_operands()?;
                    let dst = operands.dst;
                    let function_index = operands.function;
                    let parent_indices = plan.index_tail(operands.parents)?;
                    let count = parent_indices.len();
                    let parent_indices_ptr = parent_indices.as_ptr();
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(ops, 1, u64::from(view.code_block.id));
                    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
                    emit_load_u64(ops, 3, u64::from(function_index));
                    emit_load_u64(ops, 4, parent_indices_ptr as u64);
                    emit_load_u64(ops, 5, count as u64);
                    emit_call_stub(ops, jit_make_closure_stub as *const () as usize, threw);
                }
                Op::LoadString => {
                    let operands = lowered.constant_operands()?;
                    let dst = operands.dst;
                    let constant_index = operands.constant;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(ops, 1, u64::from(view.code_block.id));
                    dynasm!(ops ; .arch aarch64 ; movz x2, dst as u32);
                    emit_load_u64(ops, 3, u64::from(constant_index));
                    emit_call_stub(ops, jit_load_string_stub as *const () as usize, threw);
                }
                Op::DefineDataProperty => {
                    let operands = lowered.triple_operands()?;
                    let (object, key, value) = (operands.first, operands.second, operands.third);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, object as u32
                        ; movz x2, key as u32
                        ; movz x3, value as u32
                    );
                    emit_call_stub(
                        ops,
                        jit_define_data_property_stub as *const () as usize,
                        threw,
                    );
                }
                Op::FreshUpvalue => {
                    let idx = lowered.immediate_operands()?.value;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20);
                    emit_load_u64(ops, 1, u64::from(idx as u32));
                    emit_call_stub(ops, jit_fresh_upvalue_stub as *const () as usize, threw);
                }
                Op::LoadBuiltinError => {
                    let operands = lowered.constant_operands()?;
                    let dst = operands.dst;
                    let kind_index = operands.constant;
                    dynasm!(ops ; .arch aarch64 ; mov x0, x20 ; movz x1, dst as u32);
                    emit_load_u64(ops, 2, u64::from(kind_index));
                    emit_call_stub(
                        ops,
                        jit_load_builtin_error_stub as *const () as usize,
                        threw,
                    );
                }
                Op::Neg => {
                    let operands = lowered.unary_operands()?;
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, operands.dst as u32
                        ; movz x2, operands.src as u32
                    );
                    emit_call_stub(ops, jit_neg_stub as *const () as usize, threw);
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    emit_loose_cmp(
                        ops,
                        lowered.binary_operands()?,
                        lowered.op == Op::LooseNotEqual,
                        bail,
                    )?;
                }
                Op::DefineOwnProperty => {
                    let operands = lowered.triple_operands()?;
                    let (target, key, descriptor) =
                        (operands.first, operands.second, operands.third);
                    dynasm!(ops
                        ; .arch aarch64
                        ; mov x0, x20
                        ; movz x1, target as u32
                        ; movz x2, key as u32
                        ; movz x3, descriptor as u32
                    );
                    emit_call_stub(
                        ops,
                        jit_define_own_property_stub as *const () as usize,
                        threw,
                    );
                }
                _other => {
                    // Opcode outside the subset: bail to the interpreter at this
                    // exact PC (stamped above) instead of failing the whole
                    // compile. This lets a function with a hot, fully-supported
                    // loop tier up via OSR even when its non-loop body uses
                    // unsupported opcodes (class definition, `new`, globals,
                    // etc.). Marked `osr_only` so the function-entry path skips
                    // it (entering at PC 0 would bail immediately).
                    *osr_only = true;
                    dynasm!(ops ; .arch aarch64 ; b =>bail);
                }
            }
            // Maintain FP residency after the op. The arithmetic/compare arms
            // managed it themselves above; a load only overwrites its own
            // destination slot (so just drop that slot, preserving residency of
            // values around it in a numeric cluster); anything else is a
            // boundary or writes a slot the cache cannot track, so drop all.
            if enable_fres {
                match lowered.op {
                    Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::LessThan
                    | Op::LessEq
                    | Op::GreaterThan
                    | Op::GreaterEq
                    | Op::Equal
                    | Op::NotEqual => {}
                    Op::LoadInt32
                    | Op::LoadLocal
                    | Op::LoadNumber
                    | Op::LoadBigInt
                    | Op::LoadString
                    | Op::LoadTrue
                    | Op::LoadFalse
                    | Op::LoadUndefined
                    | Op::LoadHole => {
                        if let Some(dst) = lowered.written_register() {
                            fres.invalidate(dst);
                        }
                    }
                    _ => fres.clear(),
                }
            }
        }

        // Shared bail epilogue: status = 1, value = 0.
        dynasm!(ops
            ; .arch aarch64
            ; =>bail
            ; movz x0, #0
            ; movz x1, STATUS_BAILED as u32
        );
        emit_epilogue(ops);
        // Shared throw epilogue: status = 2 (error parked in ctx by the stub).
        dynasm!(ops
            ; .arch aarch64
            ; =>threw
            ; movz x0, #0
            ; movz x1, STATUS_THREW as u32
        );
        emit_epilogue(ops);

        // OSR trampolines: one per loop header. Each runs the standard prologue
        // (set up x19/x20 from the ctx arg) then branches to the header's body
        // label, so the VM can re-enter mid-loop with the live frame registers.
        let mut osr_entries: BTreeMap<u32, usize> = BTreeMap::new();
        for &instruction_pc in loop_headers {
            let off = ops.offset().0;
            emit_prologue(ops);
            let tgt = labels[&instruction_pc];
            dynasm!(ops ; .arch aarch64 ; b =>tgt);
            osr_entries.insert(instruction_pc, off);
        }

        plan.safepoint_records.sort_by_key(|record| record.id);
        let (register_operands, index_operands) = plan.take_operand_buffers();
        artifacts.retain_operand_buffers(register_operands, index_operands);
        let safepoint_records = plan.safepoint_records.into_boxed_slice();
        let osr_only = self.osr_only;
        let artifacts = self.artifacts.finish();
        let buf = self.ops.finalize().expect("finalize");
        Ok(BaselineCode::from_emission(
            CompiledCode::new(buf, entry),
            u64::from(view.code_block.id) + 1,
            view.code_block.register_count,
            osr_entries,
            osr_only,
            artifacts,
            safepoint_records,
            self_call_safe,
        ))
    }
}
