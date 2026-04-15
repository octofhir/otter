//! Full-RuntimeState interpreter dispatch for bytecode v2. Parallel to
//! `dispatch.rs` (which dispatches v1); feature-gated on `bytecode_v2`.
//!
//! Phase 3b lands this skeleton. The opcode set covered here is the
//! minimum subset needed to execute arithmetic functions end-to-end
//! through the *real* `RuntimeState` interpreter (not the standalone
//! harness in `bytecode_v2::dispatch_v2`). Subsequent sessions extend
//! coverage to property access, calls, generators, etc.
//!
//! Routing: `Interpreter::step_v2` is invoked from
//! `run_completion_with_runtime` when `function.bytecode_v2().is_some()`.
//! The v1 `step` remains the path for v1 bytecode.
//!
//! State conventions:
//! - Accumulator lives in `Activation::accumulator`. Every arith / compare
//!   / load op reads and/or writes it.
//! - Named register writes still go through `Activation::set_register`
//!   (which records `written_registers` for upvalue sync — v2 reuses the
//!   same open-upvalue infrastructure).
//! - PC is a byte offset into `bytecode_v2.bytes()` (not an instruction
//!   index like v1). Jumps are measured from the byte *after* the jump.

#![cfg(feature = "bytecode_v2")]

use crate::bytecode_v2::{InstructionIter, OpcodeV2, Operand};
use crate::frame::RegisterIndex;
use crate::module::{Function, Module};
use crate::value::RegisterValue;

use super::step_outcome::{StepOutcome, TailCallPayload};
use super::{Activation, FrameRuntimeState, Interpreter, InterpreterError, RuntimeState};

impl Interpreter {
    /// One-step interpreter for v2 bytecode. Parallel to
    /// `Interpreter::step` but reads from `function.bytecode_v2()` and
    /// mutates `activation.accumulator` alongside `activation.registers`.
    pub(super) fn step_v2(
        &self,
        function: &Function,
        _module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
        _frame_runtime: &mut FrameRuntimeState,
    ) -> Result<StepOutcome, InterpreterError> {
        let bytecode = function
            .bytecode_v2()
            .ok_or(InterpreterError::UnexpectedEndOfBytecode)?;
        let bytes = bytecode.bytes();

        let pc = activation.pc();
        let mut iter = InstructionIter::new(bytes);
        iter.seek(pc);
        let instr = match iter.next() {
            Some(Ok(i)) => i,
            Some(Err(_)) => return Err(InterpreterError::UnexpectedEndOfBytecode),
            None => return Err(InterpreterError::UnexpectedEndOfBytecode),
        };
        let next_pc = instr.end_pc;

        match instr.opcode {
            // ---- Accumulator load / store / move ----
            OpcodeV2::Ldar => {
                let r = reg(&instr.operands, 0)?;
                activation.set_accumulator(read_reg(activation, function,r)?);
            }
            OpcodeV2::Star => {
                let r = reg(&instr.operands, 0)?;
                write_reg(activation, function,r, activation.accumulator())?;
            }
            OpcodeV2::Mov => {
                let src = reg(&instr.operands, 0)?;
                let dst = reg(&instr.operands, 1)?;
                let v = read_reg(activation, function,src)?;
                write_reg(activation, function,dst, v)?;
            }
            OpcodeV2::LdaSmi => {
                let imm = imm(&instr.operands, 0)?;
                activation.set_accumulator(RegisterValue::from_i32(imm));
            }
            OpcodeV2::LdaUndefined => activation.set_accumulator(RegisterValue::undefined()),
            OpcodeV2::LdaNull => activation.set_accumulator(RegisterValue::null()),
            OpcodeV2::LdaTrue => activation.set_accumulator(RegisterValue::from_bool(true)),
            OpcodeV2::LdaFalse => activation.set_accumulator(RegisterValue::from_bool(false)),
            OpcodeV2::LdaTheHole => activation.set_accumulator(RegisterValue::hole()),
            OpcodeV2::LdaNaN => {
                activation.set_accumulator(
                    RegisterValue::from_raw_bits(crate::value::TAG_NAN)
                        .expect("TAG_NAN is a valid RegisterValue bit pattern"),
                );
            }
            OpcodeV2::LdaCurrentClosure => {
                if let Some(closure) = activation.closure_handle() {
                    activation.set_accumulator(RegisterValue::from_object_handle(closure.0));
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }
            OpcodeV2::LdaNewTarget => {
                if let Some(nt) = activation.construct_new_target() {
                    activation.set_accumulator(RegisterValue::from_object_handle(nt.0));
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }
            OpcodeV2::LdaConstStr => {
                let idx = idx_operand(&instr.operands, 0)?;
                use crate::string::StringId;
                let Some(s) = function.string_literals().get(StringId(idx as u16)) else {
                    return Err(InterpreterError::NativeCall(Box::from(format!(
                        "v2 LdaConstStr: string id {idx} out of range"
                    ))));
                };
                // Intern into runtime-owned JsString and box as object.
                let handle = runtime.alloc_string(s.to_string());
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            OpcodeV2::LdaConstF64 => {
                let idx = idx_operand(&instr.operands, 0)?;
                use crate::float::FloatId;
                let Some(value) = function.float_constants().get(FloatId(idx as u16)) else {
                    return Err(InterpreterError::NativeCall(Box::from(format!(
                        "v2 LdaConstF64: float id {idx} out of range"
                    ))));
                };
                activation.set_accumulator(
                    RegisterValue::from_raw_bits(value.to_bits()).unwrap_or_else(
                        RegisterValue::undefined,
                    ),
                );
            }
            OpcodeV2::LdaThis => {
                // `this` lives in the receiver slot (hidden[0]).
                if let Some(slot) = function.frame_layout().receiver_slot() {
                    let v = activation.register(slot)?;
                    activation.set_accumulator(v);
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }

            // ---- Binary arithmetic (int32 fast path; generic bail later) ----
            OpcodeV2::Add => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .add_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::Sub => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .sub_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::Mul => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .mul_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::BitwiseOr => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l | r));
            }
            OpcodeV2::BitwiseAnd => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l & r));
            }
            OpcodeV2::BitwiseXor => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l ^ r));
            }
            OpcodeV2::Shl => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                // §13.9.2 — shift amount masked to low 5 bits.
                activation.set_accumulator(RegisterValue::from_i32(
                    l.wrapping_shl((r as u32) & 0x1F),
                ));
            }
            OpcodeV2::Shr => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(
                    l.wrapping_shr((r as u32) & 0x1F),
                ));
            }
            OpcodeV2::UShr => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())? as u32;
                let r = i32_of(rhs)? as u32;
                activation.set_accumulator(RegisterValue::from_i32(
                    (l.wrapping_shr(r & 0x1F)) as i32,
                ));
            }
            OpcodeV2::Div => {
                // Int-only div: bail on non-i32 or division-by-zero
                // (v1 handles full JS semantics via runtime; Phase 3b.6
                // stays int32-only until the generic helper is wired).
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                if r == 0 {
                    return Err(InterpreterError::TypeError(Box::from(
                        "v2 Div: integer division by zero (Phase 3b.6 int-only)",
                    )));
                }
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_div(r)));
            }
            OpcodeV2::Mod => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                if r == 0 {
                    return Err(InterpreterError::TypeError(Box::from(
                        "v2 Mod: modulo by zero (Phase 3b.6 int-only)",
                    )));
                }
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_rem(r)));
            }

            // ---- Smi immediate variants ----
            OpcodeV2::AddSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_add(v)));
            }
            OpcodeV2::SubSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_sub(v)));
            }
            OpcodeV2::MulSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_mul(v)));
            }
            OpcodeV2::BitwiseOrSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l | v));
            }
            OpcodeV2::BitwiseAndSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l & v));
            }
            OpcodeV2::ShlSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(
                    l.wrapping_shl((v as u32) & 0x1F),
                ));
            }
            OpcodeV2::ShrSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(
                    l.wrapping_shr((v as u32) & 0x1F),
                ));
            }

            // ---- Unary ops on accumulator ----
            OpcodeV2::Inc => {
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_add(1)));
            }
            OpcodeV2::Dec => {
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_sub(1)));
            }
            OpcodeV2::Negate => {
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_neg()));
            }
            OpcodeV2::BitwiseNot => {
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(!l));
            }
            OpcodeV2::LogicalNot => {
                let b = activation.accumulator().is_truthy();
                activation.set_accumulator(RegisterValue::from_bool(!b));
            }
            OpcodeV2::ToBoolean => {
                let b = activation.accumulator().is_truthy();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            OpcodeV2::TypeOf => {
                let v = activation.accumulator();
                activation.set_accumulator(runtime.js_typeof(v)?);
            }

            // ---- Comparisons (int32 ordered) ----
            OpcodeV2::TestLessThan => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l < r));
            }
            OpcodeV2::TestGreaterThan => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l > r));
            }
            OpcodeV2::TestLessThanOrEqual => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l <= r));
            }
            OpcodeV2::TestGreaterThanOrEqual => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l >= r));
            }
            OpcodeV2::TestEqualStrict => {
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                activation.set_accumulator(RegisterValue::from_bool(
                    activation.accumulator() == rhs,
                ));
            }
            OpcodeV2::TestEqual => {
                // Loose equality (§7.2.14). Phase 3b.6: for int32/null/
                // undefined pairs, fall back to strict equality plus the
                // `null == undefined` special case. Number/string/object
                // coercion is deferred to Phase 3b.7 when we reuse the
                // existing `RuntimeState` coercion helpers.
                let rhs = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let lhs = activation.accumulator();
                activation.set_accumulator(RegisterValue::from_bool(loose_eq(lhs, rhs)));
            }
            OpcodeV2::TestNull => {
                let b = activation.accumulator() == RegisterValue::null();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            OpcodeV2::TestUndefined => {
                let b = activation.accumulator() == RegisterValue::undefined();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            OpcodeV2::TestUndetectable => {
                // §7.2.13: null / undefined / document.all-style values.
                // For now, equivalent to `null || undefined`.
                let v = activation.accumulator();
                let b = v == RegisterValue::null() || v == RegisterValue::undefined();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }

            // ---- Jumps (byte-offset from end_pc) ----
            OpcodeV2::Jump => {
                let off = jump_off(&instr.operands, 0)?;
                activation.set_pc(jump_target(next_pc, off));
                return Ok(StepOutcome::Continue);
            }
            OpcodeV2::JumpIfToBooleanTrue => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator().is_truthy() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfToBooleanFalse => {
                let off = jump_off(&instr.operands, 0)?;
                if !activation.accumulator().is_truthy() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfTrue => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator().as_bool() == Some(true) {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfFalse => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator().as_bool() == Some(false) {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfNull => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() == RegisterValue::null() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfNotNull => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() != RegisterValue::null() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfUndefined => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() == RegisterValue::undefined() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            OpcodeV2::JumpIfNotUndefined => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() != RegisterValue::undefined() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }

            // ---- Globals ----
            OpcodeV2::LdaGlobal => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let global_handle = runtime.intrinsics().global_object();
                match runtime.objects.get_property(global_handle, property)? {
                    Some(lookup) => {
                        let val = match lookup.value() {
                            crate::object::PropertyValue::Data { value: v, .. } => v,
                            crate::object::PropertyValue::Accessor { .. } => {
                                RegisterValue::undefined()
                            }
                        };
                        activation.set_accumulator(val);
                    }
                    None => {
                        let name = runtime.property_names().get(property).unwrap_or("?");
                        let msg = format!("{name} is not defined");
                        let error_obj = runtime.alloc_reference_error(&msg)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                            error_obj.0,
                        )));
                    }
                }
            }
            OpcodeV2::StaGlobal => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let global_handle = runtime.intrinsics().global_object();
                let value = activation.accumulator();
                runtime.objects.set_property(global_handle, property, value)?;
            }
            OpcodeV2::StaGlobalStrict => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let global_handle = runtime.intrinsics().global_object();
                if runtime
                    .objects
                    .get_property(global_handle, property)?
                    .is_none()
                {
                    let name = runtime.property_names().get(property).unwrap_or("?");
                    let msg = format!("{name} is not defined");
                    let error_obj = runtime.alloc_reference_error(&msg)?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                        error_obj.0,
                    )));
                }
                let value = activation.accumulator();
                runtime.objects.set_property(global_handle, property, value)?;
            }
            OpcodeV2::TypeOfGlobal => {
                // `typeof foo` where `foo` is an unresolvable reference
                // must NOT throw — it returns "undefined". Walk the
                // global + call the existing typeof helper.
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let global_handle = runtime.intrinsics().global_object();
                let value = match runtime.objects.get_property(global_handle, property)? {
                    Some(lookup) => match lookup.value() {
                        crate::object::PropertyValue::Data { value: v, .. } => v,
                        crate::object::PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                activation.set_accumulator(runtime.js_typeof(value)?);
            }

            // ---- Upvalues ----
            OpcodeV2::LdaUpvalue => {
                let idx = idx_operand(&instr.operands, 0)?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let cell = runtime
                    .objects
                    .closure_upvalue(closure, idx as usize)?;
                let value = runtime.objects.get_upvalue(cell)?;
                if value.is_hole() {
                    let err = runtime
                        .alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
                activation.set_accumulator(value);
            }
            OpcodeV2::StaUpvalue => {
                let idx = idx_operand(&instr.operands, 0)?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let cell = runtime
                    .objects
                    .closure_upvalue(closure, idx as usize)?;
                let value = activation.accumulator();
                runtime.objects.set_upvalue(cell, value)?;
            }

            // ---- Named property access ----
            OpcodeV2::LdaNamedProperty => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let Some(handle) = target.as_object_handle() else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "v2 LdaNamedProperty: receiver is not an object",
                    )));
                };
                let result = match runtime
                    .objects
                    .get_property(crate::object::ObjectHandle(handle), property)?
                {
                    Some(lookup) => match lookup.value() {
                        crate::object::PropertyValue::Data { value: v, .. } => v,
                        crate::object::PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                activation.set_accumulator(result);
            }
            OpcodeV2::StaNamedProperty => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let property = resolve_v2_property(function, runtime, prop_id)?;
                let Some(handle) = target.as_object_handle() else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "v2 StaNamedProperty: receiver is not an object",
                    )));
                };
                let value = activation.accumulator();
                runtime
                    .objects
                    .set_property(crate::object::ObjectHandle(handle), property, value)?;
            }

            // ---- Keyed property access ----
            //
            // v2 convention: `LdaKeyedProperty r` reads the key from the
            // accumulator and the base object from register `r`, writing
            // the fetched value back into the accumulator.
            //
            // For Phase 3b.6 we handle the common object path via
            // `runtime.property_base_object_handle` + a key → name
            // coercion through `runtime.intern_register_value_as_name`.
            // Typed-array numeric fast paths land with Phase 3b.7.
            OpcodeV2::LdaKeyedProperty => {
                let base = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let key = activation.accumulator();
                let handle = runtime.property_base_object_handle(base)?;
                let prop = key_to_property_name(runtime, key)?;
                let value = match runtime.objects.get_property(handle, prop)? {
                    Some(lookup) => match lookup.value() {
                        crate::object::PropertyValue::Data { value: v, .. } => v,
                        crate::object::PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                activation.set_accumulator(value);
            }
            OpcodeV2::StaKeyedProperty => {
                // v2: `StaKeyedProperty r0 r1`: r0[r1] = acc.
                let base = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let key = read_reg(activation, function,reg(&instr.operands, 1)?)?;
                let value = activation.accumulator();
                let handle = runtime.property_set_target_handle(base)?;
                let prop = key_to_property_name(runtime, key)?;
                runtime.objects.set_property(handle, prop, value)?;
            }

            // ---- Object / array allocation ----
            OpcodeV2::CreateObject => {
                let handle = runtime.alloc_object();
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            OpcodeV2::CreateArray => {
                let handle = runtime.alloc_array();
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }

            // ---- Coercions reusing runtime helpers ----
            OpcodeV2::ToNumber => {
                let v = activation.accumulator();
                let n = runtime.js_to_number(v)?;
                activation.set_accumulator(RegisterValue::from_number(n));
            }
            OpcodeV2::ToString => {
                let v = activation.accumulator();
                let text = runtime.js_to_string(v)?;
                let handle = runtime.alloc_string(text.into_string());
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            OpcodeV2::ToPropertyKey => {
                // §7.1.19 ToPropertyKey — keep Symbols as-is, coerce
                // everything else via ToPrimitive(hint=String) + ToString.
                let v = activation.accumulator();
                let primitive = runtime
                    .js_to_primitive_with_hint(v, super::ToPrimitiveHint::String)?;
                if primitive.as_symbol_id().is_some() {
                    activation.set_accumulator(primitive);
                } else {
                    let text = runtime.js_to_string(primitive)?;
                    let handle = runtime.alloc_string(text.into_string());
                    activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
                }
            }

            // ---- Asserts / TDZ / class guards ----
            OpcodeV2::AssertNotHole => {
                if activation.accumulator().is_hole() {
                    let err = runtime
                        .alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }
            OpcodeV2::ThrowConstAssign => {
                let err = runtime.alloc_type_error("Assignment to constant variable.")?;
                return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
            }

            // ---- Calls ----
            //
            // Phase 3b.7 coverage: `CallUndefinedReceiver`,
            // `CallAnyReceiver`, `CallProperty`, `CallDirect`. Construct,
            // TailCall, CallSpread / CallEval / CallSuper* defer to
            // Phase 3b.8+. Async / generator callables go through the
            // same `Interpreter::call_function` machinery v1 uses, so
            // plain async / sync callees "just work".
            //
            // Result of every Call op lands in the accumulator; the next
            // emitted `Star rDst` (see `transpile.rs`) moves it into the
            // destination register.
            OpcodeV2::CallUndefinedReceiver => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let (base, count) = reg_list(&instr.operands, 1)?;
                let args = read_reg_list(activation, function,base, count)?;
                let Some(handle) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match self.call_v2_callable(
                    runtime,
                    crate::object::ObjectHandle(handle),
                    RegisterValue::undefined(),
                    &args,
                ) {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.set_accumulator(value);
                    }
                    Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                    Err(other) => return Ok(other),
                }
            }
            OpcodeV2::CallAnyReceiver | OpcodeV2::CallProperty => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let receiver = read_reg(activation, function,reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function,base, count)?;
                let Some(handle) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match self.call_v2_callable(
                    runtime,
                    crate::object::ObjectHandle(handle),
                    receiver,
                    &args,
                ) {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.set_accumulator(value);
                    }
                    Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                    Err(other) => return Ok(other),
                }
            }
            OpcodeV2::CallDirect => {
                let fn_index_raw = idx_operand(&instr.operands, 0)?;
                let (base, count) = reg_list(&instr.operands, 1)?;
                let args = read_reg_list(activation, function,base, count)?;
                let callee_idx = crate::module::FunctionIndex(fn_index_raw);
                match self.call_v2_direct(runtime, _module, callee_idx, &args) {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.set_accumulator(value);
                    }
                    Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                    Err(other) => return Ok(other),
                }
            }
            // §14.6 Tail-position calls: replace the current activation
            // with the callee's frame instead of nesting. The outer loop
            // at `run_completion_with_runtime` handles
            // `StepOutcome::TailCall` by swapping module + activation +
            // function in-place. For non-plain targets (proxy, generator,
            // async, host, bound) this falls back to a regular call +
            // `StepOutcome::Return` — TCO only applies to plain bytecode
            // closures.
            //
            // Spec: <https://tc39.es/ecma262/#sec-tail-position-calls>
            OpcodeV2::TailCall => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let receiver = read_reg(activation, function,reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function,base, count)?;
                let Some(callable) = target
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };

                let is_plain_closure = matches!(
                    runtime.objects.kind(callable),
                    Ok(crate::object::HeapValueKind::Closure)
                ) && !runtime
                    .objects
                    .closure_flags(callable)
                    .is_ok_and(|f| f.is_generator() || f.is_async() || f.is_class_constructor())
                    && runtime.objects.host_function(callable)?.is_none();

                if is_plain_closure {
                    let callee_module = runtime.objects.closure_module(callable)?;
                    let callee_idx = runtime.objects.closure_callee(callable)?;
                    let callee = callee_module
                        .function(callee_idx)
                        .ok_or(InterpreterError::InvalidCallTarget)?;
                    let mut callee_activation = Activation::with_context(
                        callee_idx,
                        callee.frame_layout().register_count(),
                        crate::frame::FrameMetadata::new(
                            u16::try_from(args.len()).unwrap_or(u16::MAX),
                            crate::frame::FrameFlags::empty(),
                        ),
                        Some(callable),
                    );
                    // §10.4.4 overflow + parameter slot copy.
                    let param_count = callee.frame_layout().parameter_count();
                    for (i, &arg) in args.iter().take(param_count as usize).enumerate() {
                        let abs = callee
                            .frame_layout()
                            .resolve_user_visible(i as u16)
                            .ok_or(InterpreterError::RegisterOutOfBounds)?;
                        callee_activation.set_register(abs, arg)?;
                    }
                    if args.len() > param_count as usize {
                        callee_activation.overflow_args =
                            args[param_count as usize..].to_vec();
                    }
                    // Receiver goes into hidden slot 0 iff the callee has one.
                    if callee.frame_layout().receiver_slot().is_some() {
                        callee_activation.set_receiver(callee, receiver)?;
                    }
                    return Ok(StepOutcome::TailCall(Box::new(TailCallPayload {
                        module: callee_module,
                        activation: callee_activation,
                    })));
                } else {
                    // Non-plain target — invoke normally and return the
                    // value from *this* frame (effectively equivalent to
                    // `return f(...args)` with one extra heap frame).
                    match self.call_v2_callable(runtime, callable, receiver, &args) {
                        Ok(value) => return Ok(StepOutcome::Return(value)),
                        Err(out) => return Ok(out),
                    }
                }
            }

            // §7.3.15 Construct — `new target(...args)` with explicit
            // `new.target`. `runtime.construct_callable` handles
            // constructibility check, bound-function unwrap, host /
            // closure [[Construct]], and the return-value override
            // (§9.2.2.1) that keeps primitive returns replaced by the
            // allocated receiver.
            OpcodeV2::Construct => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let new_target = read_reg(activation, function,reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function,base, count)?;
                let Some(target_h) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not a constructor")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let Some(new_target_h) = new_target.as_object_handle() else {
                    let err = runtime.alloc_type_error("new.target is not a constructor")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match runtime.construct_callable(
                    crate::object::ObjectHandle(target_h),
                    &args,
                    crate::object::ObjectHandle(new_target_h),
                ) {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.set_accumulator(value);
                    }
                    Err(crate::VmNativeCallError::Thrown(value)) => {
                        return Ok(StepOutcome::Throw(value));
                    }
                    Err(crate::VmNativeCallError::Internal(message)) => {
                        let err = runtime.alloc_type_error(&message)?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                }
            }
            // §15.7.14 step 5.f — `class X extends Y` asserts Y is a
            // constructor. v2 variant reads the accumulator (unlike v1
            // which reads a named register), produces no value, and
            // throws `TypeError` on failure.
            OpcodeV2::AssertConstructor => {
                let v = activation.accumulator();
                let ok = v
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                    .is_some_and(|h| runtime.is_constructible(h));
                if !ok {
                    let err = runtime
                        .alloc_type_error("Class extends value is not a constructor or null")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // ---- Iteration (§5.9) ----
            //
            // `GetIterator r` — fast path through
            // `runtime.objects.alloc_iterator` which covers built-in
            // Array/String/Map/Set/arguments iterables. When the iterable
            // is not one of the fast-path kinds we surface a TypeError
            // so callers see a catchable JS error; the full Symbol.iterator
            // lookup + callable dispatch will land with Phase 3b.9b.
            OpcodeV2::GetIterator => {
                let target = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let Some(handle) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not iterable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match runtime
                    .objects
                    .alloc_iterator(crate::object::ObjectHandle(handle))
                {
                    Ok(iterator) => {
                        activation.set_accumulator(RegisterValue::from_object_handle(iterator.0));
                    }
                    Err(_) => {
                        let err = runtime.alloc_type_error("Value is not iterable")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                }
            }

            // `IteratorNext r` — step the iterator. Writes `value` into
            // the accumulator and the `done` flag into
            // `activation.secondary_result`. The compiler-emitted
            // sequence `IteratorNext r; Star r_value; <branch on
            // secondary_result>` preserves both channels.
            OpcodeV2::IteratorNext => {
                let iter_reg = reg(&instr.operands, 0)?;
                let iter_val = read_reg(activation, function,iter_reg)?;
                let Some(iterator) = iter_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("IteratorNext target is not an object")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let step = runtime.iterator_next(iterator)?;
                activation.set_accumulator(step.value());
                activation.set_secondary_result(RegisterValue::from_bool(step.is_done()));
            }

            // `IteratorClose r` — side-effectful; closes built-in
            // iterators and is a no-op for non-built-ins. Does not
            // write the accumulator (Phase 3b.9b will wire the
            // `.return()` protocol for custom iterators).
            OpcodeV2::IteratorClose => {
                let iter_val = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                if let Some(h) = iter_val.as_object_handle() {
                    let _ = runtime.objects.iterator_close(crate::object::ObjectHandle(h));
                }
            }

            // `ForInEnumerate r` — allocates a for-in property-key
            // iterator over `r` and its prototype chain. Writes the
            // iterator handle into the accumulator. `null` / `undefined`
            // source objects route to an empty iterator per §14.7.5.6
            // step 6 ("if expr is null or undefined then return break").
            OpcodeV2::ForInEnumerate => {
                let src = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let iterator = match src.as_object_handle() {
                    Some(handle) => runtime
                        .alloc_property_iterator(crate::object::ObjectHandle(handle))?,
                    None => runtime.alloc_empty_property_iterator()?,
                };
                activation.set_accumulator(RegisterValue::from_object_handle(iterator.0));
            }

            // `ForInNext value_reg iter_reg` — step the property-key
            // iterator. Writes the next key into `value_reg` (direct
            // register, not accumulator — per the transpile pattern
            // `ForInNext val iter; Star done`), and writes the done
            // flag into the accumulator so the immediately-following
            // `Star done_reg` picks it up.
            OpcodeV2::ForInNext => {
                let value_dst = reg(&instr.operands, 0)?;
                let iter_val = read_reg(activation, function,reg(&instr.operands, 1)?)?;
                let Some(iter) = iter_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    let err = runtime
                        .alloc_type_error("ForInNext target is not a property iterator")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let step = runtime.objects.property_iterator_next(iter)?;
                if step.is_done() {
                    activation.set_accumulator(RegisterValue::from_bool(true));
                } else {
                    write_reg(activation, function,value_dst, step.value())?;
                    activation.set_accumulator(RegisterValue::from_bool(false));
                }
            }

            // `ArrayPush r` — `r.push(acc)`. r must be an ordinary
            // Array object. Used by spread-emitting code. Failures
            // (not-an-array) surface as a catchable TypeError.
            OpcodeV2::ArrayPush => {
                let arr_val = read_reg(activation, function,reg(&instr.operands, 0)?)?;
                let value = activation.accumulator();
                let Some(arr) = arr_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("ArrayPush target is not an array")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                // `push_element` handles Array-kind validation and the
                // extensible / length-writable / elements-writable flags;
                // a non-Array arg surfaces as a TypeError for the user.
                if let Err(_) = runtime.objects.push_element(arr, value) {
                    let err = runtime.alloc_type_error("ArrayPush target is not an array")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // ---- Control ----
            OpcodeV2::Return => {
                return Ok(StepOutcome::Return(activation.accumulator()));
            }
            OpcodeV2::Throw => {
                return Ok(StepOutcome::Throw(activation.accumulator()));
            }
            OpcodeV2::Nop => {}

            // Any other opcode is unsupported by this Phase 3b.1
            // skeleton. Phase 3b.6 fills in property access, calls,
            // generators, etc.
            other => {
                return Err(InterpreterError::NativeCall(Box::from(format!(
                    "v2 opcode {other:?} not yet implemented by dispatch_v2"
                ))));
            }
        }

        activation.set_pc(next_pc);
        Ok(StepOutcome::Continue)
    }

    /// Invoke an arbitrary callable (host function, bytecode closure,
    /// bound function, etc.) and return the produced value. On JS throw,
    /// returns `Err(StepOutcome::Throw(value))` so the dispatcher can
    /// propagate without loss of type information.
    ///
    /// Uses `RuntimeState::call_callable` which delegates to
    /// `Interpreter::call_function` for bytecode closures — consistent
    /// with v1's internal accessor path and reused for the v2 simple
    /// call opcodes. Proxy traps, construct, and generator-start are
    /// handled by Phase 3b.8; the v1 `call_callable_for_accessor` path
    /// does not cover them either.
    fn call_v2_callable(
        &self,
        runtime: &mut RuntimeState,
        callable: crate::object::ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, StepOutcome> {
        match runtime.call_callable(callable, receiver, arguments) {
            Ok(value) => Ok(value),
            Err(crate::VmNativeCallError::Thrown(value)) => {
                Err(StepOutcome::Throw(value))
            }
            Err(crate::VmNativeCallError::Internal(message)) => {
                // Surface internal failures as TypeError so the JS program
                // can still `try { } catch` — Phase 5 will split these
                // paths to match v1 exactly.
                match runtime.alloc_type_error(&message) {
                    Ok(handle) => Err(StepOutcome::Throw(RegisterValue::from_object_handle(
                        handle.0,
                    ))),
                    Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
                }
            }
        }
    }

    /// Invoke a `CallDirect` target: callee is named by a `FunctionIndex`
    /// in the current module (same-module optimization from the v1
    /// compiler). Builds a fresh activation, copies arguments into
    /// parameter slots, runs through the tier-up hook so hotness still
    /// accrues on the callee, and returns the produced value.
    ///
    /// Unlike `call_v2_callable`, this does not need an ObjectHandle —
    /// the callee is statically known and has no captured environment
    /// (direct calls target top-level / non-capturing functions).
    fn call_v2_direct(
        &self,
        runtime: &mut RuntimeState,
        module: &Module,
        callee_idx: crate::module::FunctionIndex,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, StepOutcome> {
        use crate::frame::{FrameFlags, FrameMetadata};

        let callee = match module.function(callee_idx) {
            Some(f) => f,
            None => {
                match runtime.alloc_type_error("CallDirect: invalid function index") {
                    Ok(h) => {
                        return Err(StepOutcome::Throw(RegisterValue::from_object_handle(h.0)));
                    }
                    Err(_) => return Err(StepOutcome::Throw(RegisterValue::undefined())),
                }
            }
        };
        let register_count = callee.frame_layout().register_count();
        let argc = u16::try_from(arguments.len()).unwrap_or(u16::MAX);
        let mut activation = Activation::with_context(
            callee_idx,
            register_count,
            FrameMetadata::new(argc, FrameFlags::empty()),
            None,
        );
        // Copy args into user-visible parameter slots.
        let param_count = callee.frame_layout().parameter_count();
        for (i, &arg) in arguments.iter().take(param_count as usize).enumerate() {
            let abs = match callee.frame_layout().resolve_user_visible(i as u16) {
                Some(a) => a,
                None => {
                    return Err(StepOutcome::Throw(RegisterValue::undefined()));
                }
            };
            if activation.set_register(abs, arg).is_err() {
                return Err(StepOutcome::Throw(RegisterValue::undefined()));
            }
        }
        // Preserve overflow args for CreateArguments (§10.4.4).
        if arguments.len() > param_count as usize {
            activation.overflow_args = arguments[param_count as usize..].to_vec();
        }

        match self.run_with_tier_up(module, &mut activation, runtime) {
            Ok(super::Completion::Return(v)) => Ok(v),
            Ok(super::Completion::Throw(v)) => Err(StepOutcome::Throw(v)),
            Err(InterpreterError::UncaughtThrow(v)) => Err(StepOutcome::Throw(v)),
            Err(InterpreterError::TypeError(msg)) => {
                match runtime.alloc_type_error(&msg) {
                    Ok(h) => Err(StepOutcome::Throw(RegisterValue::from_object_handle(h.0))),
                    Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
                }
            }
            Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
        }
    }
}

// -------- operand / helper plumbing --------

fn reg(ops: &[Operand], pos: usize) -> Result<RegisterIndex, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Reg(r)) => RegisterIndex::try_from(*r)
            .map_err(|_| InterpreterError::RegisterOutOfBounds),
        _ => Err(InterpreterError::NativeCall(
            Box::from("v2 operand kind mismatch: expected Reg"),
        )),
    }
}

fn imm(ops: &[Operand], pos: usize) -> Result<i32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Imm(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(
            Box::from("v2 operand kind mismatch: expected Imm"),
        )),
    }
}

fn idx_operand(ops: &[Operand], pos: usize) -> Result<u32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Idx(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(
            Box::from("v2 operand kind mismatch: expected Idx"),
        )),
    }
}

/// Decode a `RegList` operand into `(base, count)`.
fn reg_list(ops: &[Operand], pos: usize) -> Result<(u32, u32), InterpreterError> {
    match ops.get(pos) {
        Some(Operand::RegList { base, count }) => Ok((*base, *count)),
        _ => Err(InterpreterError::NativeCall(Box::from(
            "v2 operand kind mismatch: expected RegList",
        ))),
    }
}

/// Read a contiguous register range (as named by a `RegList`) into an
/// owned `Vec<RegisterValue>` for call-site argument vectors.
/// Goes through `read_bytecode_register` so register indices are
/// user-visible (adjusted for hidden-slot offset).
fn read_reg_list(
    activation: &Activation,
    function: &Function,
    base: u32,
    count: u32,
) -> Result<Vec<RegisterValue>, InterpreterError> {
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let r = RegisterIndex::try_from(base.checked_add(i).ok_or(
            InterpreterError::RegisterOutOfBounds,
        )?)
        .map_err(|_| InterpreterError::RegisterOutOfBounds)?;
        out.push(activation.read_bytecode_register(function, r)?);
    }
    Ok(out)
}

/// Loose equality (§7.2.14) for the subset the Phase 3b.6 dispatcher
/// handles: strict equality plus the `null == undefined` special case.
/// Full numeric / string coercion lands with Phase 3b.7 once
/// `RuntimeState::js_loose_equals` is wired here.
fn loose_eq(a: RegisterValue, b: RegisterValue) -> bool {
    if a == b {
        return true;
    }
    let a_is_null_or_undef = a == RegisterValue::null() || a == RegisterValue::undefined();
    let b_is_null_or_undef = b == RegisterValue::null() || b == RegisterValue::undefined();
    a_is_null_or_undef && b_is_null_or_undef
}

/// Resolve a `PropertyNameId` into a runtime-interned id via the
/// function's property-name side table. Mirrors
/// `Interpreter::resolve_property_name` from v1 dispatch but takes a
/// raw u32 (the v2 `Idx` operand) instead of a v1 `RegisterIndex`.
fn resolve_v2_property(
    function: &Function,
    runtime: &mut RuntimeState,
    raw_id: u32,
) -> Result<crate::property::PropertyNameId, InterpreterError> {
    let id = crate::property::PropertyNameId(raw_id as u16);
    let property_name = function
        .property_names()
        .get(id)
        .ok_or(InterpreterError::UnknownPropertyName)?;
    Ok(runtime.intern_property_name(property_name))
}

/// Coerce a `RegisterValue` to a property-name id. Strings intern
/// directly; numbers/other primitives stringify via `js_to_string`.
/// Symbols are not yet handled (they live in an orthogonal namespace
/// that Phase 3b.7 will wire through).
fn key_to_property_name(
    runtime: &mut RuntimeState,
    key: RegisterValue,
) -> Result<crate::property::PropertyNameId, InterpreterError> {
    // Fast path: key is already a string object — pull its text out.
    if let Some(handle) = key.as_object_handle()
        && let Some(s) = runtime
            .objects
            .string_value(crate::object::ObjectHandle(handle))?
    {
        let owned = s.to_string();
        return Ok(runtime.intern_property_name(&owned));
    }
    // Fallback: stringify via the runtime's existing ToString.
    let text = runtime.js_to_string(key)?;
    Ok(runtime.intern_property_name(&text))
}

fn jump_off(ops: &[Operand], pos: usize) -> Result<i32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::JumpOff(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(
            Box::from("v2 operand kind mismatch: expected JumpOff"),
        )),
    }
}

/// Read a v2 register operand. v2 register indices are **user-visible**
/// (same convention as v1) so they pass through the frame layout's
/// hidden-slot offset before hitting the raw register file. Without
/// this resolution a function with a receiver slot (`hidden_count > 0`)
/// would have v2 `Ldar r0` read the receiver instead of parameter 0.
fn read_reg(
    act: &Activation,
    function: &Function,
    index: RegisterIndex,
) -> Result<RegisterValue, InterpreterError> {
    act.read_bytecode_register(function, index)
}

fn write_reg(
    act: &mut Activation,
    function: &Function,
    index: RegisterIndex,
    value: RegisterValue,
) -> Result<(), InterpreterError> {
    act.write_bytecode_register(function, index, value)
}

fn i32_of(v: RegisterValue) -> Result<i32, InterpreterError> {
    v.as_i32().ok_or_else(|| {
        InterpreterError::TypeError(Box::from("operand expected int32 in v2 dispatch"))
    })
}

fn jump_target(end_pc: u32, offset: i32) -> u32 {
    let t = i64::from(end_pc) + i64::from(offset);
    u32::try_from(t).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::bytecode_v2::{BytecodeBuilder, OpcodeV2, Operand};
    use crate::frame::FrameLayout;
    use crate::module::{Function, FunctionIndex, Module};
    use crate::value::RegisterValue;

    use super::super::{Interpreter, RuntimeState};

    /// Runs a v2-only function against the real `RuntimeState` +
    /// `Interpreter` pipeline: builds a Module with a single Function,
    /// attaches the v2 bytecode, calls `execute_with_runtime`, and
    /// returns the resulting value.
    fn run_v2(
        v2_build: impl FnOnce(&mut BytecodeBuilder),
        register_count: u16,
        initial_regs: &[RegisterValue],
    ) -> RegisterValue {
        let mut builder = BytecodeBuilder::new();
        v2_build(&mut builder);
        let v2 = builder.finish().expect("build v2 bytecode");
        let layout = FrameLayout::new(0, 0, register_count, 0).expect("layout");
        let function = Function::with_bytecode(Some("test"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("valid module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), initial_regs, &mut runtime)
            .expect("execute_with_runtime");
        result.return_value()
    }

    /// Like `run_v2`, but lets the caller preseed the property-name and
    /// string-literal side tables. Used by the tests that exercise
    /// named/keyed property access opcodes.
    fn run_v2_with_tables(
        v2_build: impl FnOnce(&mut BytecodeBuilder),
        register_count: u16,
        initial_regs: &[RegisterValue],
        property_names: Vec<&'static str>,
        string_literals: Vec<&'static str>,
    ) -> (RegisterValue, RuntimeState) {
        use crate::bigint::BigIntTable;
        use crate::call::CallTable;
        use crate::closure::ClosureTable;
        use crate::float::FloatTable;
        use crate::module::{FunctionSideTables, FunctionTables};
        use crate::property::PropertyNameTable;
        use crate::regexp::RegExpTable;
        use crate::string::StringTable;

        let mut builder = BytecodeBuilder::new();
        v2_build(&mut builder);
        let v2 = builder.finish().expect("build v2 bytecode");
        let layout = FrameLayout::new(0, 0, register_count, 0).expect("layout");
        let side_tables = FunctionSideTables::new(
            PropertyNameTable::new(property_names),
            StringTable::new(string_literals),
            FloatTable::default(),
            BigIntTable::default(),
            ClosureTable::default(),
            CallTable::default(),
            RegExpTable::default(),
        );
        let tables = FunctionTables::new(
            side_tables,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let function = Function::new(Some("test"), layout, Default::default(), tables)
            .with_bytecode_v2(v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("valid module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), initial_regs, &mut runtime)
            .expect("execute_with_runtime");
        (result.return_value(), runtime)
    }

    #[test]
    fn return_smi_literal_through_real_runtime() {
        // LdaSmi 42; Return. Returns acc = 42.
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(42)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn add_two_regs_via_accumulator() {
        // r0 = 10, r1 = 32; Ldar r0; Add r1; Return (→ 42).
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(OpcodeV2::Add, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            2,
            &[RegisterValue::from_i32(10), RegisterValue::from_i32(32)],
        );
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn smi_variants_compute_correctly() {
        // (((5 + 3) - 2) * 2) | 0 = 12
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(5)]).unwrap();
                b.emit(OpcodeV2::AddSmi, &[Operand::Imm(3)]).unwrap();
                b.emit(OpcodeV2::SubSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(OpcodeV2::MulSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(OpcodeV2::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(12));
    }

    #[test]
    fn shifts_mask_to_low_five_bits() {
        // 0xDEAD_BEEF_u32 as i32 >>> 4 = 0x0DEA_DBEE
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(0xDEAD_BEEFu32 as i32)])
                    .unwrap();
                b.emit(OpcodeV2::ShrSmi, &[Operand::Imm(4)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        // `Shr` is signed (>> in JS), so 0xDEADBEEF >> 4 = 0xFDEADBEE.
        assert_eq!(result.as_i32(), Some(0xFDEADBEEu32 as i32));
    }

    #[test]
    fn logical_not_inverts_truthiness() {
        // !true = false, !0 = true, !42 = false.
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::LdaTrue, &[]).unwrap();
                b.emit(OpcodeV2::LogicalNot, &[]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_bool(), Some(false));
    }

    #[test]
    fn inc_dec_on_accumulator() {
        // 10; Inc; Inc; Dec; Return → 11
        let result = run_v2(
            |b| {
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(10)]).unwrap();
                b.emit(OpcodeV2::Inc, &[]).unwrap();
                b.emit(OpcodeV2::Inc, &[]).unwrap();
                b.emit(OpcodeV2::Dec, &[]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(11));
    }

    #[test]
    fn null_undefined_jumps() {
        // if (acc === null) return 1 else return 2.
        let result = run_v2(
            |b| {
                let then_label = b.new_label();
                b.emit(OpcodeV2::LdaNull, &[]).unwrap();
                b.emit_jump_to(OpcodeV2::JumpIfNull, then_label).unwrap();
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
                b.bind_label(then_label).unwrap();
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(1)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(1));
    }

    #[test]
    fn div_and_mod() {
        // 17 / 5 = 3, 17 % 5 = 2.  Chain them: (17/5) + (17%5) = 5.
        let result = run_v2(
            |b| {
                // r0 = 17, r1 = 5
                // tmp = r0 / r1
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(OpcodeV2::Div, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();
                // acc = r0 % r1
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(OpcodeV2::Mod, &[Operand::Reg(1)]).unwrap();
                // acc = acc + tmp
                b.emit(OpcodeV2::Add, &[Operand::Reg(2)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            3,
            &[
                RegisterValue::from_i32(17),
                RegisterValue::from_i32(5),
                RegisterValue::undefined(),
            ],
        );
        assert_eq!(result.as_i32(), Some(5));
    }

    #[test]
    fn create_object_and_get_set_named_property() {
        // const o = {}; o.x = 7; return o.x + 5  →  12
        //
        // PropertyNameTable is pre-seeded with "x" at PropertyNameId(0).
        let (result, _runtime) = run_v2_with_tables(
            |b| {
                b.emit(OpcodeV2::CreateObject, &[]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(0)]).unwrap();
                // o.x = 7
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(7)]).unwrap();
                b.emit(
                    OpcodeV2::StaNamedProperty,
                    &[Operand::Reg(0), Operand::Idx(0)],
                )
                .unwrap();
                // acc = o.x
                b.emit(
                    OpcodeV2::LdaNamedProperty,
                    &[Operand::Reg(0), Operand::Idx(0)],
                )
                .unwrap();
                b.emit(OpcodeV2::AddSmi, &[Operand::Imm(5)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            1,
            &[RegisterValue::undefined()],
            vec!["x"],
            vec![],
        );
        assert_eq!(result.as_i32(), Some(12));
    }

    #[test]
    fn keyed_property_access_via_string_key() {
        // const o = {}; const k = "y"; o[k] = 100; return o[k]  →  100
        //
        // StringTable is pre-seeded with "y" at StringId(0); the
        // property-name table is pre-seeded with "y" so that the
        // runtime-interned id matches across store/load.
        let (result, _runtime) = run_v2_with_tables(
            |b| {
                // r0 = {}
                b.emit(OpcodeV2::CreateObject, &[]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(0)]).unwrap();
                // r1 = "y"
                b.emit(OpcodeV2::LdaConstStr, &[Operand::Idx(0)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
                // acc = 100; o[r1] = acc
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(100)]).unwrap();
                b.emit(
                    OpcodeV2::StaKeyedProperty,
                    &[Operand::Reg(0), Operand::Reg(1)],
                )
                .unwrap();
                // acc = r1; LdaKeyedProperty r0 → acc = r0[acc]
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::LdaKeyedProperty, &[Operand::Reg(0)])
                    .unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            2,
            &[RegisterValue::undefined(), RegisterValue::undefined()],
            vec!["y"],
            vec!["y"],
        );
        assert_eq!(result.as_i32(), Some(100));
    }

    #[test]
    fn assert_not_hole_throws_on_hole() {
        // LdaTheHole; AssertNotHole; Return (unreachable) → Throw.
        let mut builder = BytecodeBuilder::new();
        builder.emit(OpcodeV2::LdaTheHole, &[]).unwrap();
        builder.emit(OpcodeV2::AssertNotHole, &[]).unwrap();
        builder.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let err = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime)
            .unwrap_err();
        assert!(matches!(
            err,
            crate::interpreter::InterpreterError::UncaughtThrow(_)
        ));
    }

    #[test]
    fn typeof_number_returns_number_string() {
        // typeof 42 === "number"
        let mut builder = BytecodeBuilder::new();
        builder.emit(OpcodeV2::LdaSmi, &[Operand::Imm(42)]).unwrap();
        builder.emit(OpcodeV2::TypeOf, &[]).unwrap();
        builder.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime)
            .expect("execute");
        let text = runtime.js_to_string_infallible(result.return_value());
        assert_eq!(text.as_ref(), "number");
    }

    #[test]
    fn loop_sum_through_real_runtime() {
        // function(n) { let s=0,i=0; while(i<n){ s=(s+i)|0; i+=1; } return s; }
        // Register file: r0 = n (param), r1 = s, r2 = i.
        //
        // Layout (byte PCs shown after prefix decisions):
        //   pc0:  LdaSmi 0
        //   pc2:  Star r1              ; s = 0
        //   pc4:  LdaSmi 0
        //   pc6:  Star r2              ; i = 0
        //   loop_header (bind here):
        //   pcL:  Ldar r2
        //   pcL+2: TestLessThan r0     ; acc = (i < n)
        //   pcL+4: JumpIfToBooleanFalse -> exit
        //   ... body:
        //        Ldar r1
        //        Add r2                ; acc = s + i
        //        BitwiseOrSmi 0        ; acc |= 0
        //        Star r1               ; s = acc
        //        Ldar r2
        //        AddSmi 1              ; acc = i + 1
        //        Star r2               ; i = acc
        //        Jump loop_header
        //   exit (bind here):
        //        Ldar r1
        //        Return
        let result = run_v2(
            |b| {
                // init s=0, i=0
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();

                let loop_header = b.new_label();
                let exit = b.new_label();
                b.bind_label(loop_header).unwrap();
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(2)]).unwrap();
                b.emit(OpcodeV2::TestLessThan, &[Operand::Reg(0)]).unwrap();
                b.emit_jump_to(OpcodeV2::JumpIfToBooleanFalse, exit).unwrap();

                // body: s = (s + i) | 0
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::Add, &[Operand::Reg(2)]).unwrap();
                b.emit(OpcodeV2::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();

                // i = i + 1
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(2)]).unwrap();
                b.emit(OpcodeV2::AddSmi, &[Operand::Imm(1)]).unwrap();
                b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();

                b.emit_jump_to(OpcodeV2::Jump, loop_header).unwrap();

                b.bind_label(exit).unwrap();
                b.emit(OpcodeV2::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(OpcodeV2::Return, &[]).unwrap();
            },
            3,
            &[
                RegisterValue::from_i32(100),
                RegisterValue::undefined(),
                RegisterValue::undefined(),
            ],
        );
        // sum(0..99) = 99*100/2 = 4950.
        assert_eq!(result.as_i32(), Some(4950));
    }

    /// CallDirect: caller invokes a statically-known callee by
    /// FunctionIndex. The callee takes two parameters and returns their
    /// sum. End-to-end through `run_with_tier_up`, so the same path the
    /// JIT will eventually drive.
    #[test]
    fn call_direct_adds_two_params() {
        // Callee (fn_index 1): function(a, b) { return a + b; }
        // Register layout: r0=a, r1=b. Parameter slots are user-visible
        // 0 and 1 per `FrameLayout::new(2 params, 0 hidden, 0 locals, 0 scratch)`.
        let mut callee_b = BytecodeBuilder::new();
        callee_b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::Add, &[Operand::Reg(1)]).unwrap();
        callee_b.emit(OpcodeV2::Return, &[]).unwrap();
        let callee_v2 = callee_b.finish().unwrap();
        // FrameLayout::new(hidden, params, locals, temps) — 2 params, no hidden slots.
        let callee_layout = FrameLayout::new(0, 2, 0, 0).unwrap();
        let callee = Function::with_bytecode(Some("sum"), callee_layout, Default::default())
            .with_bytecode_v2(callee_v2);

        // Caller (fn_index 0): function() {
        //   r0 = 10; r1 = 32;
        //   acc = CallDirect(fn_index=1, args=[r0, r1]);
        //   return acc;
        // }
        // Register layout: r0/r1 are the arg slots (no params, 0 hidden, 2 locals).
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(10)]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(0)]).unwrap();
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(32)]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                OpcodeV2::CallDirect,
                &[
                    Operand::Idx(1),
                    Operand::RegList { base: 0, count: 2 },
                ],
            )
            .unwrap();
        caller_b.emit(OpcodeV2::Return, &[]).unwrap();
        let caller_v2 = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_bytecode(Some("main"), caller_layout, Default::default())
            .with_bytecode_v2(caller_v2);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result = match interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime)
        {
            Ok(r) => r,
            Err(crate::interpreter::InterpreterError::UncaughtThrow(v)) => {
                let text = runtime.js_to_string_infallible(v);
                panic!("unexpected throw from CallDirect: {}", text.as_ref());
            }
            Err(e) => panic!("execute: {e:?}"),
        };
        assert_eq!(result.return_value().as_i32(), Some(42));
    }

    /// CallUndefinedReceiver: caller invokes a bytecode closure stored
    /// in a register, with an undefined `this`. Exercises the
    /// `runtime.call_callable` path which delegates to the same
    /// `Interpreter::call_function` v1 uses for host / closure dispatch.
    #[test]
    fn call_undefined_receiver_invokes_closure() {
        use crate::object::ClosureFlags as ObjClosureFlags;

        // Callee (fn_index 1): function double(x) { return x + x; }
        let mut callee_b = BytecodeBuilder::new();
        callee_b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::Add, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::Return, &[]).unwrap();
        let callee_v2 = callee_b.finish().unwrap();
        let callee_layout = FrameLayout::new(0, 1, 0, 0).unwrap();
        let callee = Function::with_bytecode(Some("double"), callee_layout, Default::default())
            .with_bytecode_v2(callee_v2);

        // Caller (fn_index 0):
        //   r0 = <closure>       (preseeded)
        //   r1 = 21
        //   acc = Call r0(undefined, [r1])
        //   return acc
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(21)]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                OpcodeV2::CallUndefinedReceiver,
                &[
                    Operand::Reg(0),
                    Operand::RegList { base: 1, count: 1 },
                ],
            )
            .unwrap();
        caller_b.emit(OpcodeV2::Return, &[]).unwrap();
        let caller_v2 = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_bytecode(Some("main"), caller_layout, Default::default())
            .with_bytecode_v2(caller_v2);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");

        // Build the runtime, enter the module (so alloc_closure can find it),
        // and allocate a closure pointing at fn_index 1. Stuff the resulting
        // handle into the caller's r0 via `execute_with_runtime`'s preseed
        // argument list.
        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle = runtime.alloc_closure(
            FunctionIndex(1),
            Vec::new(),
            ObjClosureFlags::default(),
        );
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
        {
            Ok(r) => r,
            Err(crate::interpreter::InterpreterError::UncaughtThrow(v)) => {
                let text = runtime.js_to_string_infallible(v);
                panic!("unexpected throw: {}", text.as_ref());
            }
            Err(e) => panic!("execute: {e:?}"),
        };
        assert_eq!(result.return_value().as_i32(), Some(42));
    }

    /// ForInEnumerate + ForInNext walks an object's own property keys.
    /// Builds an object with `{a: 1, b: 2}`, allocates a property
    /// iterator, steps twice, and checks that both keys were returned
    /// before done=true. Secondary is routed through the accumulator
    /// per the v2 convention (`done` in acc, value directly to dst reg).
    #[test]
    fn for_in_enumerate_walks_property_keys() {
        // Preseed r0 = {a: 1, b: 2}. Loop:
        //   ForInEnumerate r0 → acc = iter
        //   Star r1                ; r1 = iter
        //   ForInNext r2 r1        ; r2 = key, acc = done
        //   Star r3                ; r3 = done1
        //   ForInNext r4 r1        ; r4 = key, acc = done
        //   Star r5                ; r5 = done2
        //   ForInNext r6 r1        ; r6 = key, acc = done
        //   Ldar r3                ; acc = done1 (false)
        //   Return
        let mut b = BytecodeBuilder::new();
        b.emit(OpcodeV2::ForInEnumerate, &[Operand::Reg(0)])
            .unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(
            OpcodeV2::ForInNext,
            &[Operand::Reg(2), Operand::Reg(1)],
        )
        .unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(3)]).unwrap();
        b.emit(
            OpcodeV2::ForInNext,
            &[Operand::Reg(4), Operand::Reg(1)],
        )
        .unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(5)]).unwrap();
        b.emit(
            OpcodeV2::ForInNext,
            &[Operand::Reg(6), Operand::Reg(1)],
        )
        .unwrap();
        // acc is now `true` (done on third step). Return it.
        b.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 7, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let obj = runtime.alloc_object();
        let a_id = runtime.intern_property_name("a");
        let b_id = runtime.intern_property_name("b");
        runtime
            .objects
            .set_property(obj, a_id, RegisterValue::from_i32(1))
            .unwrap();
        runtime
            .objects
            .set_property(obj, b_id, RegisterValue::from_i32(2))
            .unwrap();
        let preseed = [RegisterValue::from_object_handle(obj.0)];

        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
            .expect("execute");
        // Third ForInNext: iterator exhausted, acc = done = true.
        assert_eq!(result.return_value().as_bool(), Some(true));
    }

    /// ArrayPush appends the accumulator onto an array in a register.
    /// Pre-seeds a freshly-allocated empty array into r0 and issues
    /// three `LdaSmi N; ArrayPush r0` pairs. Then reads length off the
    /// array to verify the final state.
    #[test]
    fn array_push_appends_accumulator_to_array() {
        let mut b = BytecodeBuilder::new();
        // ArrayPush reads the array from Reg(0), pushes acc. We'll
        // preseed r0 with a JS array.
        b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(10)]).unwrap();
        b.emit(OpcodeV2::ArrayPush, &[Operand::Reg(0)]).unwrap();
        b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(20)]).unwrap();
        b.emit(OpcodeV2::ArrayPush, &[Operand::Reg(0)]).unwrap();
        b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(30)]).unwrap();
        b.emit(OpcodeV2::ArrayPush, &[Operand::Reg(0)]).unwrap();
        // Return the array itself (acc = Ldar r0).
        b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
        b.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 1, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let arr = runtime.alloc_array();
        let preseed = [RegisterValue::from_object_handle(arr.0)];

        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
            .expect("execute");
        // Returned value is the same array. Verify length == 3 and
        // each element matches.
        let arr_h = crate::object::ObjectHandle(
            result
                .return_value()
                .as_object_handle()
                .expect("array handle"),
        );
        let elements = runtime.objects.array_elements(arr_h).expect("elements");
        assert_eq!(elements.len(), 3);
        for (i, expected) in [10, 20, 30].iter().enumerate() {
            assert_eq!(
                elements[i].as_i32(),
                Some(*expected),
                "index {i} mismatch"
            );
        }
    }

    /// GetIterator + IteratorNext walks a built-in array iterator end to
    /// end. Preseeds an array `[100, 200]` into r0, builds:
    ///   GetIterator r0 → acc = iter
    ///   Star r1                   ; r1 = iter
    ///   IteratorNext r1           ; acc = 100, secondary = false
    ///   Star r2                   ; r2 = first value
    ///   IteratorNext r1           ; acc = 200
    ///   Star r3
    ///   IteratorNext r1           ; acc = undefined, secondary = true
    ///   Ldar r2; Add r3; Return   ; returns 300
    #[test]
    fn get_iterator_and_iterator_next_walk_array() {
        let mut b = BytecodeBuilder::new();
        b.emit(OpcodeV2::GetIterator, &[Operand::Reg(0)]).unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(OpcodeV2::IteratorNext, &[Operand::Reg(1)]).unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();
        b.emit(OpcodeV2::IteratorNext, &[Operand::Reg(1)]).unwrap();
        b.emit(OpcodeV2::Star, &[Operand::Reg(3)]).unwrap();
        b.emit(OpcodeV2::IteratorNext, &[Operand::Reg(1)]).unwrap();
        // acc is now undefined; ignore. Compute r2 + r3.
        b.emit(OpcodeV2::Ldar, &[Operand::Reg(2)]).unwrap();
        b.emit(OpcodeV2::Add, &[Operand::Reg(3)]).unwrap();
        b.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 4, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let arr = runtime.alloc_array();
        // Populate arr with [100, 200] via `push_element` (the
        // element-aware path; `set_property` would fail because
        // indexed properties on arrays route through the elements vec,
        // not the named-property storage).
        runtime
            .objects
            .push_element(arr, RegisterValue::from_i32(100))
            .unwrap();
        runtime
            .objects
            .push_element(arr, RegisterValue::from_i32(200))
            .unwrap();
        let preseed = [RegisterValue::from_object_handle(arr.0)];

        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
            .expect("execute");
        assert_eq!(result.return_value().as_i32(), Some(300));
    }

    /// TailCall replaces the caller's frame with the callee's. Used by
    /// `return f(...)` in tail position to avoid unbounded stack growth.
    /// Here `main()` tail-calls `double(10)` which returns 20 — we verify
    /// the value bubbles up cleanly (the tail-swap is transparent to
    /// the test, but the exercise catches activation-swap bugs).
    #[test]
    fn tail_call_invokes_closure_and_returns_its_value() {
        use crate::object::ClosureFlags as ObjClosureFlags;

        // Callee: function double(x) { return x + x; }
        let mut callee_b = BytecodeBuilder::new();
        callee_b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::Add, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::Return, &[]).unwrap();
        let callee_v2 = callee_b.finish().unwrap();
        let callee_layout = FrameLayout::new(0, 1, 0, 0).unwrap();
        let callee = Function::with_bytecode(Some("double"), callee_layout, Default::default())
            .with_bytecode_v2(callee_v2);

        // Caller: r0 = closure, r1 = 10, r2 = undefined (receiver);
        //         TailCall(r0, r2, [r1]).
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(10)]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        caller_b.emit(OpcodeV2::LdaUndefined, &[]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();
        caller_b
            .emit(
                OpcodeV2::TailCall,
                &[
                    Operand::Reg(0),
                    Operand::Reg(2),
                    Operand::RegList { base: 1, count: 1 },
                ],
            )
            .unwrap();
        // Dead code — TailCall should have replaced our frame.
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(-1)]).unwrap();
        caller_b.emit(OpcodeV2::Return, &[]).unwrap();
        let caller_v2 = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 3, 0).unwrap();
        let caller = Function::with_bytecode(Some("main"), caller_layout, Default::default())
            .with_bytecode_v2(caller_v2);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle = runtime.alloc_closure(
            FunctionIndex(1),
            Vec::new(),
            ObjClosureFlags::default(),
        );
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
        {
            Ok(r) => r,
            Err(crate::interpreter::InterpreterError::UncaughtThrow(v)) => {
                let text = runtime.js_to_string_infallible(v);
                panic!("unexpected throw: {}", text.as_ref());
            }
            Err(e) => panic!("execute: {e:?}"),
        };
        assert_eq!(result.return_value().as_i32(), Some(20));
    }

    /// AssertConstructor throws when the accumulator is not callable
    /// with `[[Construct]]`. Here we load `42` (a number) into the
    /// accumulator and the guard should refuse it as a superclass.
    #[test]
    fn assert_constructor_throws_on_non_constructor() {
        let mut builder = BytecodeBuilder::new();
        builder.emit(OpcodeV2::LdaSmi, &[Operand::Imm(42)]).unwrap();
        builder.emit(OpcodeV2::AssertConstructor, &[]).unwrap();
        builder.emit(OpcodeV2::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_bytecode(Some("t"), layout, Default::default())
            .with_bytecode_v2(v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let err = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime)
            .unwrap_err();
        assert!(matches!(
            err,
            crate::interpreter::InterpreterError::UncaughtThrow(_)
        ));
    }

    /// Construct: `new F(7)` where F is a preseeded closure that reads
    /// `this` and stores `arg0 * 2` onto it. Expected: result is the
    /// allocated receiver object, and the closure's `this` persisted
    /// the multiplied value (we verify by reading the property back).
    ///
    /// Uses a preseeded closure just like `call_undefined_receiver_invokes_closure`
    /// — a full v2 CreateClosure flow would first require property-name
    /// side tables, which arrive with Phase 3b.11.
    #[test]
    fn construct_preseeded_closure_returns_receiver() {
        use crate::object::ClosureFlags as ObjClosureFlags;
        use crate::property::PropertyNameTable;
        use crate::module::{FunctionSideTables, FunctionTables};
        use crate::bigint::BigIntTable;
        use crate::call::CallTable;
        use crate::closure::ClosureTable;
        use crate::float::FloatTable;
        use crate::regexp::RegExpTable;
        use crate::string::StringTable;

        // Callee `function F(n) { this.x = n * 2; }` — parameter n in
        // user-visible r0, two user-visible locals (r1 = n*2, r2 = this).
        // Frame layout: 1 hidden (`this`), 1 param (n), 2 locals, 0 temp.
        //
        //   Ldar r0               ; acc = n
        //   MulSmi 2              ; acc = n*2
        //   Star r1               ; r1 = n*2
        //   LdaThis               ; acc = this
        //   Star r2               ; r2 = this
        //   Ldar r1               ; acc = n*2 (the value)
        //   StaNamedProperty r2, PropertyNameId(0="x")  ; this.x = acc
        //   LdaUndefined; Return
        let x_name_table = PropertyNameTable::new(vec!["x"]);
        let mut callee_b = BytecodeBuilder::new();
        callee_b.emit(OpcodeV2::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(OpcodeV2::MulSmi, &[Operand::Imm(2)]).unwrap();
        callee_b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        callee_b.emit(OpcodeV2::LdaThis, &[]).unwrap();
        callee_b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();
        callee_b.emit(OpcodeV2::Ldar, &[Operand::Reg(1)]).unwrap();
        callee_b
            .emit(
                OpcodeV2::StaNamedProperty,
                &[Operand::Reg(2), Operand::Idx(0)],
            )
            .unwrap();
        callee_b.emit(OpcodeV2::LdaUndefined, &[]).unwrap();
        callee_b.emit(OpcodeV2::Return, &[]).unwrap();
        let callee_v2 = callee_b.finish().unwrap();
        let callee_layout = FrameLayout::new(1, 1, 2, 0).unwrap();
        let callee_side_tables = FunctionSideTables::new(
            x_name_table,
            StringTable::default(),
            FloatTable::default(),
            BigIntTable::default(),
            ClosureTable::default(),
            CallTable::default(),
            RegExpTable::default(),
        );
        let callee_tables = FunctionTables::new(
            callee_side_tables,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
        );
        let callee = Function::new(Some("F"), callee_layout, Default::default(), callee_tables)
            .with_bytecode_v2(callee_v2);

        // Caller: return new F(7)
        //   r0 = <closure F>      (preseeded)
        //   r1 = 7
        //   acc = Construct(r0, new_target=r0, [r1])
        //   return acc
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(7)]).unwrap();
        caller_b.emit(OpcodeV2::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                OpcodeV2::Construct,
                &[
                    Operand::Reg(0),
                    Operand::Reg(0),
                    Operand::RegList { base: 1, count: 1 },
                ],
            )
            .unwrap();
        caller_b.emit(OpcodeV2::Return, &[]).unwrap();
        let caller_v2 = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_bytecode(Some("main"), caller_layout, Default::default())
            .with_bytecode_v2(caller_v2);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");

        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle = runtime.alloc_closure(
            FunctionIndex(1),
            Vec::new(),
            ObjClosureFlags::default(),
        );
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &preseed, &mut runtime)
        {
            Ok(r) => r,
            Err(crate::interpreter::InterpreterError::UncaughtThrow(v)) => {
                let text = runtime.js_to_string_infallible(v);
                panic!("unexpected throw: {}", text.as_ref());
            }
            Err(e) => panic!("execute: {e:?}"),
        };
        // Construct returns the allocated receiver object. Read x off of it
        // through the runtime's get_property.
        let recv = result.return_value();
        let recv_handle = recv
            .as_object_handle()
            .expect("Construct should return receiver object");
        let x_id = runtime.intern_property_name("x");
        let lookup = runtime
            .objects
            .get_property(crate::object::ObjectHandle(recv_handle), x_id)
            .expect("get_property")
            .expect("x present");
        match lookup.value() {
            crate::object::PropertyValue::Data { value, .. } => {
                assert_eq!(value.as_i32(), Some(14));
            }
            other => panic!("expected data property, got {other:?}"),
        }
    }

    /// CallDirect propagates a JS throw from the callee back into the
    /// caller's dispatcher as `UncaughtThrow`.
    #[test]
    fn call_direct_propagates_throw() {
        // Callee (fn_index 1): throw 7
        let mut callee_b = BytecodeBuilder::new();
        callee_b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(7)]).unwrap();
        callee_b.emit(OpcodeV2::Throw, &[]).unwrap();
        let callee_v2 = callee_b.finish().unwrap();
        let callee = Function::with_bytecode(
            Some("bomb"),
            FrameLayout::new(0, 0, 0, 0).unwrap(),
            Default::default(),
        )
        .with_bytecode_v2(callee_v2);

        // Caller (fn_index 0): return CallDirect(1, [])
        let mut caller_b = BytecodeBuilder::new();
        caller_b
            .emit(
                OpcodeV2::CallDirect,
                &[
                    Operand::Idx(1),
                    Operand::RegList { base: 0, count: 0 },
                ],
            )
            .unwrap();
        caller_b.emit(OpcodeV2::Return, &[]).unwrap();
        let caller_v2 = caller_b.finish().unwrap();
        let caller = Function::with_bytecode(
            Some("main"),
            FrameLayout::new(0, 0, 0, 0).unwrap(),
            Default::default(),
        )
        .with_bytecode_v2(caller_v2);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let err = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime)
            .unwrap_err();
        match err {
            crate::interpreter::InterpreterError::UncaughtThrow(v) => {
                assert_eq!(v.as_i32(), Some(7));
            }
            other => panic!("expected UncaughtThrow(7), got {other:?}"),
        }
    }
}
