//! MIR → Cranelift IR translation.
//!
//! Translates each MIR instruction into one or more Cranelift IR instructions.
//! If any instruction cannot be lowered, compilation fails with `JitError`
//! and the function stays in the interpreter.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    AbiParam, Block, Function as ClifFunction, InstBuilder, MemFlags, Signature, Value,
};
use cranelift_codegen::isa::TargetIsa;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use crate::abi::jit_function_signature;
use crate::codegen::value_repr;
use crate::context::offsets;
use crate::mir::graph::{BlockId, MirGraph, ValueId};
use crate::mir::nodes::MirOp;
use crate::mir::types::CmpOp;
use crate::{BAILOUT_SENTINEL, JitError};

/// Lower a complete MIR graph into a Cranelift IR function.
pub fn lower_mir_to_clif(graph: &MirGraph, isa: &dyn TargetIsa) -> Result<ClifFunction, JitError> {
    let call_conv = isa.default_call_conv();
    let pointer_type = isa.pointer_type();

    let sig = jit_function_signature(call_conv, pointer_type);
    let mut func =
        ClifFunction::with_name_signature(cranelift_codegen::ir::UserFuncName::user(0, 0), sig);

    let mut func_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut func_ctx);

    let mut block_map: HashMap<BlockId, Block> = HashMap::new();
    for mir_block in &graph.blocks {
        let clif_block = builder.create_block();
        block_map.insert(mir_block.id, clif_block);
    }

    let entry_clif_block = block_map[&graph.entry_block];
    builder.append_block_params_for_function_params(entry_clif_block);
    builder.switch_to_block(entry_clif_block);
    builder.seal_block(entry_clif_block);

    let ctx_ptr = builder.block_params(entry_clif_block)[0];

    // Cache registers_base pointer once at function entry — avoids redundant
    // loads from JitContext on every LoadLocal/StoreLocal.
    let registers_base = builder.ins().load(
        pointer_type,
        MemFlags::trusted(),
        ctx_ptr,
        crate::context::offsets::REGISTERS_BASE,
    );

    let mut value_map: HashMap<ValueId, Value> = HashMap::new();
    let mut emitted_predecessors: HashMap<BlockId, usize> = HashMap::new();

    let mut lowerer = OpLowerer {
        ctx_ptr,
        registers_base,
        pointer_type,
        value_map: &mut value_map,
        block_map: &block_map,
        emitted_predecessors: &mut emitted_predecessors,
        graph,
    };

    for (block_idx, mir_block) in graph.blocks.iter().enumerate() {
        let clif_block = block_map[&mir_block.id];

        if block_idx > 0 {
            builder.switch_to_block(clif_block);
            if mir_block.predecessors.is_empty() {
                builder.seal_block(clif_block);
            }
        }

        // Track if current block has been terminated (by a guard's deopt/continue
        // split or by an unsupported op that we turned into a bailout).
        let mut block_terminated = false;

        for instr in &mir_block.instrs {
            if block_terminated {
                // After a terminator, skip remaining instructions in this MIR block.
                // They'll be dead code (the MIR builder handles this via Deopt).
                break;
            }

            let result = lowerer.lower_op(&mut builder, &instr.op, instr.bytecode_pc)?;

            if let Some(clif_val) = result {
                lowerer.value_map.insert(instr.value, clif_val);
            }

            // Check if we just emitted a terminator
            if instr.op.is_terminator() {
                block_terminated = true;
            }
        }
    }

    builder.finalize();
    Ok(func)
}

/// Holds state for lowering operations.
struct OpLowerer<'a> {
    ctx_ptr: Value,
    registers_base: Value,
    pointer_type: types::Type,
    value_map: &'a mut HashMap<ValueId, Value>,
    block_map: &'a HashMap<BlockId, Block>,
    emitted_predecessors: &'a mut HashMap<BlockId, usize>,
    graph: &'a MirGraph,
}

impl<'a> OpLowerer<'a> {
    /// Get the CLIF Value for a MIR ValueId.
    fn v(&self, id: &ValueId) -> Result<Value, JitError> {
        self.value_map.get(id).copied().ok_or_else(|| {
            JitError::Internal(format!(
                "MIR value {} not available in current CLIF block",
                id
            ))
        })
    }

    /// Get CLIF Block for a MIR BlockId.
    fn b(&self, id: &BlockId) -> Block {
        self.block_map[id]
    }

    fn register_edge(&mut self, builder: &mut FunctionBuilder, target: BlockId) {
        let count = self.emitted_predecessors.entry(target).or_insert(0);
        *count += 1;
        if *count == self.graph.block(target).predecessors.len() {
            builder.seal_block(self.b(&target));
        }
    }

    /// Lower a single MIR operation.
    fn lower_op(
        &mut self,
        builder: &mut FunctionBuilder,
        op: &MirOp,
        bytecode_pc: u32,
    ) -> Result<Option<Value>, JitError> {
        let ctx_ptr = self.ctx_ptr;
        let pointer_type = self.pointer_type;

        match op {
            // ---- Constants ----
            MirOp::Const(bits) => Ok(Some(builder.ins().iconst(types::I64, *bits as i64))),
            MirOp::Undefined => Ok(Some(value_repr::emit_undefined(builder))),
            MirOp::Null => Ok(Some(value_repr::emit_null(builder))),
            MirOp::True => Ok(Some(value_repr::emit_true(builder))),
            MirOp::False => Ok(Some(value_repr::emit_false(builder))),
            MirOp::ConstInt32(n) => Ok(Some(builder.ins().iconst(types::I32, *n as i64))),
            MirOp::ConstFloat64(n) => Ok(Some(builder.ins().f64const(*n))),

            // ---- Boxing ----
            MirOp::BoxInt32(val) => Ok(Some(value_repr::emit_box_int32(builder, self.v(val)?))),
            MirOp::BoxFloat64(val) => Ok(Some(value_repr::emit_box_float64(builder, self.v(val)?))),
            MirOp::BoxBool(val) => Ok(Some(value_repr::emit_box_bool(builder, self.v(val)?))),
            MirOp::UnboxInt32(val) => Ok(Some(value_repr::emit_unbox_int32(builder, self.v(val)?))),
            MirOp::UnboxFloat64(val) => {
                Ok(Some(value_repr::emit_unbox_float64(builder, self.v(val)?)))
            }
            MirOp::Int32ToFloat64(val) => {
                Ok(Some(builder.ins().fcvt_from_sint(types::F64, self.v(val)?)))
            }

            // ---- Guards ----
            MirOp::GuardInt32 { val, deopt } => {
                let boxed = self.v(val)?;
                let is_int = value_repr::emit_is_int32(builder, boxed);
                self.emit_guard(
                    builder,
                    is_int,
                    deopt,
                    crate::BailoutReason::TypeGuardFailed,
                )?;
                Ok(Some(value_repr::emit_unbox_int32(builder, boxed)))
            }
            MirOp::GuardFloat64 { val, deopt } => {
                let boxed = self.v(val)?;
                let is_f64 = value_repr::emit_is_float64(builder, boxed);
                self.emit_guard(
                    builder,
                    is_f64,
                    deopt,
                    crate::BailoutReason::TypeGuardFailed,
                )?;
                Ok(Some(value_repr::emit_unbox_float64(builder, boxed)))
            }
            MirOp::GuardObject { val, deopt } => {
                let boxed = self.v(val)?;
                let is_obj = value_repr::emit_is_object(builder, boxed);
                self.emit_guard(
                    builder,
                    is_obj,
                    deopt,
                    crate::BailoutReason::TypeGuardFailed,
                )?;
                Ok(Some(value_repr::emit_extract_pointer(builder, boxed)))
            }
            MirOp::GuardShape { .. }
            | MirOp::GuardProtoEpoch { .. }
            | MirOp::GuardArrayDense { .. }
            | MirOp::GuardBoundsCheck { .. }
            | MirOp::GuardNotHole { .. }
            | MirOp::GuardString { .. }
            | MirOp::GuardFunction { .. }
            | MirOp::GuardBool { .. } => {
                // TODO: implement these guards properly
                Ok(None)
            }

            // ---- Int32 Arithmetic (overflow → deopt) ----
            MirOp::AddI32 { lhs, rhs, deopt } => {
                let (result, overflow) = builder.ins().sadd_overflow(self.v(lhs)?, self.v(rhs)?);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }
            MirOp::SubI32 { lhs, rhs, deopt } => {
                let (result, overflow) = builder.ins().ssub_overflow(self.v(lhs)?, self.v(rhs)?);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }
            MirOp::MulI32 { lhs, rhs, deopt } => {
                let (result, overflow) = builder.ins().smul_overflow(self.v(lhs)?, self.v(rhs)?);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }
            MirOp::DivI32 { lhs, rhs, deopt } => {
                let l = self.v(lhs)?;
                let r = self.v(rhs)?;
                let zero = builder.ins().iconst(types::I32, 0);
                let is_zero = builder.ins().icmp(IntCC::Equal, r, zero);
                // Deopt on div-by-zero (overflow=true means deopt)
                self.emit_overflow_guard(builder, is_zero, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(builder.ins().sdiv(l, r)))
            }
            MirOp::ModI32 { lhs, rhs, deopt } => {
                let l = self.v(lhs)?;
                let r = self.v(rhs)?;
                let zero = builder.ins().iconst(types::I32, 0);
                let is_zero = builder.ins().icmp(IntCC::Equal, r, zero);
                self.emit_overflow_guard(builder, is_zero, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(builder.ins().srem(l, r)))
            }
            MirOp::IncI32 { val, deopt } => {
                let one = builder.ins().iconst(types::I32, 1);
                let (result, overflow) = builder.ins().sadd_overflow(self.v(val)?, one);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }
            MirOp::DecI32 { val, deopt } => {
                let one = builder.ins().iconst(types::I32, 1);
                let (result, overflow) = builder.ins().ssub_overflow(self.v(val)?, one);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }
            MirOp::NegI32 { val, deopt } => {
                let zero = builder.ins().iconst(types::I32, 0);
                let (result, overflow) = builder.ins().ssub_overflow(zero, self.v(val)?);
                self.emit_overflow_guard(builder, overflow, deopt, crate::BailoutReason::Overflow)?;
                Ok(Some(result))
            }

            // ---- Float64 Arithmetic ----
            MirOp::AddF64 { lhs, rhs } => Ok(Some(builder.ins().fadd(self.v(lhs)?, self.v(rhs)?))),
            MirOp::SubF64 { lhs, rhs } => Ok(Some(builder.ins().fsub(self.v(lhs)?, self.v(rhs)?))),
            MirOp::MulF64 { lhs, rhs } => Ok(Some(builder.ins().fmul(self.v(lhs)?, self.v(rhs)?))),
            MirOp::DivF64 { lhs, rhs } => Ok(Some(builder.ins().fdiv(self.v(lhs)?, self.v(rhs)?))),
            MirOp::ModF64 { .. } => Err(JitError::UnsupportedInstruction(
                "ModF64 (needs libcall)".into(),
            )),
            MirOp::NegF64(val) => Ok(Some(builder.ins().fneg(self.v(val)?))),

            // ---- Bitwise ----
            MirOp::BitAnd { lhs, rhs } => Ok(Some(builder.ins().band(self.v(lhs)?, self.v(rhs)?))),
            MirOp::BitOr { lhs, rhs } => Ok(Some(builder.ins().bor(self.v(lhs)?, self.v(rhs)?))),
            MirOp::BitXor { lhs, rhs } => Ok(Some(builder.ins().bxor(self.v(lhs)?, self.v(rhs)?))),
            MirOp::Shl { lhs, rhs } => Ok(Some(builder.ins().ishl(self.v(lhs)?, self.v(rhs)?))),
            MirOp::Shr { lhs, rhs } => Ok(Some(builder.ins().sshr(self.v(lhs)?, self.v(rhs)?))),
            MirOp::Ushr { lhs, rhs } => Ok(Some(builder.ins().ushr(self.v(lhs)?, self.v(rhs)?))),
            MirOp::BitNot(val) => Ok(Some(builder.ins().bnot(self.v(val)?))),

            // ---- Comparisons ----
            MirOp::CmpI32 { op, lhs, rhs } => Ok(Some(builder.ins().icmp(
                cmp_to_intcc(op),
                self.v(lhs)?,
                self.v(rhs)?,
            ))),
            MirOp::CmpF64 { op, lhs, rhs } => Ok(Some(builder.ins().fcmp(
                cmp_to_floatcc(op),
                self.v(lhs)?,
                self.v(rhs)?,
            ))),
            MirOp::CmpStrictEq { lhs, rhs } => Ok(Some(builder.ins().icmp(
                IntCC::Equal,
                self.v(lhs)?,
                self.v(rhs)?,
            ))),
            MirOp::CmpStrictNe { lhs, rhs } => Ok(Some(builder.ins().icmp(
                IntCC::NotEqual,
                self.v(lhs)?,
                self.v(rhs)?,
            ))),
            MirOp::LogicalNot(val) => {
                let zero = builder.ins().iconst(types::I8, 0);
                Ok(Some(builder.ins().icmp(IntCC::Equal, self.v(val)?, zero)))
            }
            MirOp::IsTruthy(val) => {
                let v = self.v(val)?;
                let false_bits = builder
                    .ins()
                    .iconst(types::I64, value_repr::TAG_FALSE as i64);
                let undef_bits = builder
                    .ins()
                    .iconst(types::I64, value_repr::TAG_UNDEFINED as i64);
                let null_bits = builder
                    .ins()
                    .iconst(types::I64, value_repr::TAG_NULL as i64);
                let zero_bits = builder
                    .ins()
                    .iconst(types::I64, value_repr::TAG_INT32 as i64);

                let not_false = builder.ins().icmp(IntCC::NotEqual, v, false_bits);
                let not_undef = builder.ins().icmp(IntCC::NotEqual, v, undef_bits);
                let not_null = builder.ins().icmp(IntCC::NotEqual, v, null_bits);
                let not_zero = builder.ins().icmp(IntCC::NotEqual, v, zero_bits);

                let a = builder.ins().band(not_false, not_undef);
                let b = builder.ins().band(a, not_null);
                Ok(Some(builder.ins().band(b, not_zero)))
            }

            // ---- Variables ----
            MirOp::LoadLocal(idx) => {
                let offset = (*idx as i32) * 8;
                Ok(Some(builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    self.registers_base,
                    offset,
                )))
            }
            MirOp::StoreLocal { idx, val } => {
                let offset = (*idx as i32) * 8;
                builder
                    .ins()
                    .store(MemFlags::trusted(), self.v(val)?, self.registers_base, offset);
                Ok(None)
            }
            MirOp::LoadRegister(idx) => {
                let base = self.registers_base;
                let local_count = builder.ins().load(
                    types::I32,
                    MemFlags::trusted(),
                    ctx_ptr,
                    offsets::LOCAL_COUNT,
                );
                let local_count_64 = builder.ins().uextend(types::I64, local_count);
                let idx_val = builder.ins().iconst(types::I64, *idx as i64);
                let total_idx = builder.ins().iadd(local_count_64, idx_val);
                let byte_offset = builder.ins().imul_imm(total_idx, 8);
                let addr = builder.ins().iadd(base, byte_offset);
                Ok(Some(builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    addr,
                    0,
                )))
            }
            MirOp::StoreRegister { idx, val } => {
                let base = self.registers_base;
                let local_count = builder.ins().load(
                    types::I32,
                    MemFlags::trusted(),
                    ctx_ptr,
                    offsets::LOCAL_COUNT,
                );
                let local_count_64 = builder.ins().uextend(types::I64, local_count);
                let idx_val = builder.ins().iconst(types::I64, *idx as i64);
                let total_idx = builder.ins().iadd(local_count_64, idx_val);
                let byte_offset = builder.ins().imul_imm(total_idx, 8);
                let addr = builder.ins().iadd(base, byte_offset);
                builder
                    .ins()
                    .store(MemFlags::trusted(), self.v(val)?, addr, 0);
                Ok(None)
            }
            MirOp::LoadThis => Ok(Some(builder.ins().load(
                types::I64,
                MemFlags::trusted(),
                ctx_ptr,
                offsets::THIS_RAW,
            ))),

            // ---- Property fast paths ----
            MirOp::GetPropShaped {
                obj,
                shape_id,
                offset,
                ..
            } => {
                let obj = self.to_i64(builder, self.v(obj)?);
                let shape = builder.ins().iconst(types::I64, *shape_id as i64);
                let offset = builder.ins().iconst(types::I64, i64::from(*offset));
                let pc = builder.ins().iconst(types::I64, i64::from(bytecode_pc));
                let result = self.emit_direct_host_call(
                    builder,
                    crate::runtime_helpers::otter_get_prop_shaped as *const () as usize,
                    &[obj, shape, offset, pc],
                );
                let result = self.emit_return_if_bailout_sentinel(builder, result);
                Ok(Some(result))
            }
            MirOp::SetPropShaped {
                obj,
                shape_id,
                offset,
                val,
                ..
            } => {
                let obj = self.to_i64(builder, self.v(obj)?);
                let shape = builder.ins().iconst(types::I64, *shape_id as i64);
                let offset = builder.ins().iconst(types::I64, i64::from(*offset));
                let value = self.to_i64(builder, self.v(val)?);
                let pc = builder.ins().iconst(types::I64, i64::from(bytecode_pc));
                let result = self.emit_direct_host_call(
                    builder,
                    crate::runtime_helpers::otter_set_prop_shaped as *const () as usize,
                    &[obj, shape, offset, value, pc],
                );
                let _ = self.emit_return_if_bailout_sentinel(builder, result);
                Ok(None)
            }

            // ---- Generic property access (cold path, calls runtime helper) ----
            MirOp::GetPropConstGeneric {
                obj,
                name_idx,
                ..
            } => {
                let obj = self.to_i64(builder, self.v(obj)?);
                let prop_id = builder.ins().iconst(types::I64, i64::from(*name_idx));
                let pc = builder.ins().iconst(types::I64, i64::from(bytecode_pc));
                let result = self.emit_direct_host_call(
                    builder,
                    crate::runtime_helpers::otter_get_prop_generic as *const () as usize,
                    &[obj, prop_id, pc],
                );
                let result = self.emit_return_if_bailout_sentinel(builder, result);
                Ok(Some(result))
            }
            MirOp::SetPropConstGeneric {
                obj,
                name_idx,
                val,
                ..
            } => {
                let obj = self.to_i64(builder, self.v(obj)?);
                let prop_id = builder.ins().iconst(types::I64, i64::from(*name_idx));
                let value = self.to_i64(builder, self.v(val)?);
                let pc = builder.ins().iconst(types::I64, i64::from(bytecode_pc));
                let result = self.emit_direct_host_call(
                    builder,
                    crate::runtime_helpers::otter_set_prop_generic as *const () as usize,
                    &[obj, prop_id, value, pc],
                );
                let _ = self.emit_return_if_bailout_sentinel(builder, result);
                Ok(None)
            }

            // ---- Control Flow ----
            MirOp::Jump(target) => {
                builder.ins().jump(self.b(target), &[]);
                self.register_edge(builder, *target);
                Ok(None)
            }
            MirOp::Branch {
                cond,
                true_block,
                false_block,
            } => {
                builder.ins().brif(
                    self.v(cond)?,
                    self.b(true_block),
                    &[],
                    self.b(false_block),
                    &[],
                );
                self.register_edge(builder, *true_block);
                self.register_edge(builder, *false_block);
                Ok(None)
            }
            MirOp::Return(val) => {
                builder.ins().return_(&[self.v(val)?]);
                Ok(None)
            }
            MirOp::ReturnUndefined => {
                let undef = value_repr::emit_undefined(builder);
                builder.ins().return_(&[undef]);
                Ok(None)
            }
            MirOp::CallDirect { target, args } => {
                let callee_index = self.to_i64(builder, self.v(target)?);
                let argc = builder
                    .ins()
                    .iconst(types::I64, i64::try_from(args.len()).unwrap_or(i64::MAX));
                let undefined = builder.ins().iconst(
                    types::I64,
                    otter_vm::RegisterValue::undefined().raw_bits() as i64,
                );
                let mut call_args = vec![
                    callee_index,
                    builder.ins().iconst(types::I64, i64::from(bytecode_pc)),
                    argc,
                ];
                for arg in args.iter().take(8) {
                    call_args.push(self.to_i64(builder, self.v(arg)?));
                }
                while call_args.len() < 11 {
                    call_args.push(undefined);
                }
                let result = self.emit_direct_host_call(
                    builder,
                    crate::runtime_helpers::otter_call_direct as *const () as usize,
                    &call_args,
                );
                let result = self.emit_return_if_bailout_sentinel(builder, result);
                Ok(Some(result))
            }
            MirOp::Deopt(deopt) => {
                let deopt_info = &self.graph.deopts[deopt.0 as usize];
                emit_bailout(
                    builder,
                    ctx_ptr,
                    deopt_info.bytecode_pc,
                    crate::BailoutReason::Unsupported,
                );
                Ok(None)
            }

            // ---- Void operations (no codegen needed) ----
            MirOp::CloseUpvalue(_)
            | MirOp::WriteBarrier(_)
            | MirOp::TryStart { .. }
            | MirOp::TryEnd => Ok(None),
            MirOp::Safepoint { .. } => {
                self.emit_safepoint(builder, bytecode_pc);
                Ok(None)
            }

            // ---- Helper calls (cold exits to runtime) ----
            MirOp::HelperCall { kind, args } => {
                let helper_name = helper_kind_to_symbol(kind);
                self.emit_helper_call(builder, helper_name, args)
            }

            // ---- Everything else: cannot lower → compilation fails ----
            _ => {
                let op_name = format!("{:?}", op);
                let short = if op_name.len() > 60 {
                    &op_name[..60]
                } else {
                    &op_name
                };
                Err(JitError::UnsupportedInstruction(short.to_string()))
            }
        }
    }

    /// Emit a type guard: if `condition` is false, deopt.
    /// Switches builder to a fresh continue_block.
    fn emit_guard(
        &self,
        builder: &mut FunctionBuilder,
        condition: Value,
        deopt: &crate::mir::graph::DeoptId,
        reason: crate::BailoutReason,
    ) -> Result<(), JitError> {
        let deopt_info = &self.graph.deopts[deopt.0 as usize];
        let continue_block = builder.create_block();
        let deopt_block = builder.create_block();

        builder
            .ins()
            .brif(condition, continue_block, &[], deopt_block, &[]);

        builder.switch_to_block(deopt_block);
        builder.seal_block(deopt_block);
        emit_bailout(builder, self.ctx_ptr, deopt_info.bytecode_pc, reason);

        builder.switch_to_block(continue_block);
        builder.seal_block(continue_block);
        Ok(())
    }

    /// Emit an overflow guard: if `overflow` is true, deopt.
    fn emit_overflow_guard(
        &self,
        builder: &mut FunctionBuilder,
        overflow: Value,
        deopt: &crate::mir::graph::DeoptId,
        reason: crate::BailoutReason,
    ) -> Result<(), JitError> {
        let deopt_info = &self.graph.deopts[deopt.0 as usize];
        let continue_block = builder.create_block();
        let deopt_block = builder.create_block();

        builder
            .ins()
            .brif(overflow, deopt_block, &[], continue_block, &[]);

        builder.switch_to_block(deopt_block);
        builder.seal_block(deopt_block);
        emit_bailout(builder, self.ctx_ptr, deopt_info.bytecode_pc, reason);

        builder.switch_to_block(continue_block);
        builder.seal_block(continue_block);
        Ok(())
    }

    fn emit_safepoint(&self, builder: &mut FunctionBuilder, bytecode_pc: u32) {
        let flag_ptr = builder.ins().load(
            self.pointer_type,
            MemFlags::trusted(),
            self.ctx_ptr,
            offsets::INTERRUPT_FLAG,
        );
        let null_ptr = builder.ins().iconst(self.pointer_type, 0);
        let ptr_is_null = builder.ins().icmp(IntCC::Equal, flag_ptr, null_ptr);

        let continue_block = builder.create_block();
        let poll_block = builder.create_block();
        let deopt_block = builder.create_block();

        builder
            .ins()
            .brif(ptr_is_null, continue_block, &[], poll_block, &[]);

        builder.switch_to_block(poll_block);
        builder.seal_block(poll_block);
        let raw = builder
            .ins()
            .load(types::I8, MemFlags::trusted(), flag_ptr, 0);
        let zero = builder.ins().iconst(types::I8, 0);
        let is_set = builder.ins().icmp(IntCC::NotEqual, raw, zero);
        builder
            .ins()
            .brif(is_set, deopt_block, &[], continue_block, &[]);

        builder.switch_to_block(deopt_block);
        builder.seal_block(deopt_block);
        emit_bailout(
            builder,
            self.ctx_ptr,
            bytecode_pc,
            crate::BailoutReason::Interrupted,
        );

        builder.switch_to_block(continue_block);
        builder.seal_block(continue_block);
    }

    fn to_i64(&self, builder: &mut FunctionBuilder, value: Value) -> Value {
        match builder.func.dfg.value_type(value) {
            types::I64 => value,
            types::I32 | types::I8 | types::I16 => builder.ins().uextend(types::I64, value),
            ty if ty == self.pointer_type => {
                debug_assert_eq!(self.pointer_type, types::I64);
                value
            }
            _ => value,
        }
    }

    fn emit_direct_host_call(
        &self,
        builder: &mut FunctionBuilder,
        addr: usize,
        args: &[Value],
    ) -> Value {
        let call_conv = builder.func.signature.call_conv;
        let mut sig = Signature::new(call_conv);
        sig.params.push(AbiParam::new(self.pointer_type));
        for _ in args {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        let sig_ref = builder.import_signature(sig);
        let addr_val = builder.ins().iconst(self.pointer_type, addr as i64);

        let mut call_args = Vec::with_capacity(1 + args.len());
        call_args.push(self.ctx_ptr);
        call_args.extend_from_slice(args);

        let call = builder.ins().call_indirect(sig_ref, addr_val, &call_args);
        builder.inst_results(call)[0]
    }

    fn emit_return_if_bailout_sentinel(
        &self,
        builder: &mut FunctionBuilder,
        result: Value,
    ) -> Value {
        let continue_block = builder.create_block();
        let bailout_block = builder.create_block();
        let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL as i64);
        let is_bailout = builder.ins().icmp(IntCC::Equal, result, sentinel);
        builder
            .ins()
            .brif(is_bailout, bailout_block, &[], continue_block, &[]);

        builder.switch_to_block(bailout_block);
        builder.seal_block(bailout_block);
        builder.ins().return_(&[sentinel]);

        builder.switch_to_block(continue_block);
        builder.seal_block(continue_block);
        result
    }

    /// Emit a call to a runtime helper function.
    ///
    /// All helpers have signature: `extern "C" fn(*mut JitContext, i64...) -> i64`
    /// The ctx_ptr is always passed as the first argument.
    fn emit_helper_call(
        &self,
        builder: &mut FunctionBuilder,
        symbol_name: &str,
        args: &[ValueId],
    ) -> Result<Option<Value>, JitError> {
        let call_conv = builder.func.signature.call_conv;

        // Build the helper signature: (ptr, i64, i64, ...) -> i64
        let mut sig = Signature::new(call_conv);
        sig.params.push(AbiParam::new(self.pointer_type)); // ctx
        for _ in args {
            sig.params.push(AbiParam::new(types::I64)); // NaN-boxed args
        }
        sig.returns.push(AbiParam::new(types::I64)); // NaN-boxed result

        let sig_ref = builder.import_signature(sig);

        // Look up the helper function address from registered symbols.
        let addr = crate::pipeline::lookup_helper_address(symbol_name).ok_or_else(|| {
            JitError::Internal(format!("helper symbol '{}' not registered", symbol_name))
        })?;

        let addr_val = builder.ins().iconst(self.pointer_type, addr as i64);

        // Build arguments: [ctx_ptr, arg0, arg1, ...]
        let mut call_args = Vec::with_capacity(1 + args.len());
        call_args.push(self.ctx_ptr);
        for arg_id in args {
            call_args.push(self.v(arg_id)?);
        }

        let call = builder.ins().call_indirect(sig_ref, addr_val, &call_args);
        let result = builder.inst_results(call)[0];
        Ok(Some(result))
    }
}

fn emit_bailout(
    builder: &mut FunctionBuilder,
    ctx_ptr: Value,
    bytecode_pc: u32,
    reason: crate::BailoutReason,
) {
    let reason_val = builder.ins().iconst(types::I32, reason as i64);
    builder.ins().store(
        MemFlags::trusted(),
        reason_val,
        ctx_ptr,
        offsets::BAILOUT_REASON,
    );
    let pc_val = builder.ins().iconst(types::I32, bytecode_pc as i64);
    builder
        .ins()
        .store(MemFlags::trusted(), pc_val, ctx_ptr, offsets::BAILOUT_PC);
    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL as i64);
    builder.ins().return_(&[sentinel]);
}

fn cmp_to_intcc(op: &CmpOp) -> IntCC {
    match op {
        CmpOp::Eq => IntCC::Equal,
        CmpOp::Ne => IntCC::NotEqual,
        CmpOp::Lt => IntCC::SignedLessThan,
        CmpOp::Le => IntCC::SignedLessThanOrEqual,
        CmpOp::Gt => IntCC::SignedGreaterThan,
        CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
    }
}

fn cmp_to_floatcc(op: &CmpOp) -> FloatCC {
    match op {
        CmpOp::Eq => FloatCC::Equal,
        CmpOp::Ne => FloatCC::NotEqual,
        CmpOp::Lt => FloatCC::LessThan,
        CmpOp::Le => FloatCC::LessThanOrEqual,
        CmpOp::Gt => FloatCC::GreaterThan,
        CmpOp::Ge => FloatCC::GreaterThanOrEqual,
    }
}

/// Map MIR HelperKind to the extern "C" symbol name.
fn helper_kind_to_symbol(kind: &crate::mir::nodes::HelperKind) -> &'static str {
    use crate::mir::nodes::HelperKind;
    match kind {
        HelperKind::GenericAdd => "otter_jit_generic_add",
        HelperKind::GenericSub => "otter_jit_generic_sub",
        HelperKind::GenericMul => "otter_jit_generic_mul",
        HelperKind::GenericDiv => "otter_jit_generic_div",
        HelperKind::GenericMod => "otter_jit_generic_mod",
        HelperKind::GenericNeg => "otter_jit_generic_neg",
        HelperKind::GenericInc => "otter_jit_generic_inc",
        HelperKind::GenericDec => "otter_jit_generic_dec",
        HelperKind::GenericEq => "otter_jit_generic_eq",
        HelperKind::GenericStrictEq => "otter_jit_generic_eq", // reuse for now
        HelperKind::GenericLt => "otter_jit_generic_lt",
        HelperKind::GenericLe => "otter_jit_generic_le",
        HelperKind::GenericGt => "otter_jit_generic_gt",
        HelperKind::GenericGe => "otter_jit_generic_ge",
        HelperKind::Pow => "otter_jit_generic_pow",
        // These don't have helpers yet — will cause UnsupportedInstruction
        // at a higher level (the MIR ops that use them are not HelperCall).
        _ => "otter_jit_unsupported",
    }
}
