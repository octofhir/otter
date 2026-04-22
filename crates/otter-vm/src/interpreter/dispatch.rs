//! Bytecode interpreter dispatch.
//!
//! Drives the Ignition-style accumulator ISA through `RuntimeState`.
//! The opcode set covered here is the minimum subset needed to execute
//! arithmetic functions, property access, calls, and generators
//! end-to-end. Opcodes outside that subset return
//! [`InterpreterError::UnexpectedEndOfBytecode`] or a more specific
//! diagnostic; coverage grows per milestone.
//!
//! Routing: [`Interpreter::step`] is invoked from
//! `run_completion_with_runtime` once per interpreter tick.
//!
//! State conventions:
//! - Accumulator lives in `Activation::accumulator`. Every arith / compare
//!   / load op reads and/or writes it.
//! - Named register writes still go through `Activation::set_register`
//!   (which records `written_registers` for upvalue sync — the
//!   open-upvalue infrastructure is shared across all dispatchers).
//! - PC is a byte offset into `bytecode.bytes()`; jumps are measured
//!   from the byte *after* the jump operand.

use crate::bytecode::{InstructionIter, Opcode, Operand};
use crate::feedback::ArithmeticFeedback;
use crate::frame::RegisterIndex;
use crate::module::{Function, Module};
use crate::value::RegisterValue;

use super::activation::{PendingAbruptCompletion, UsingEntry};
use super::step_outcome::{StepOutcome, TailCallPayload};
use super::{Activation, FrameRuntimeState, Interpreter, InterpreterError, RuntimeState};

impl Interpreter {
    /// One-step interpreter for v2 bytecode. Parallel to
    /// `Interpreter::step` but reads from `function.bytecode()` and
    /// mutates `activation.accumulator` alongside `activation.registers`.
    pub(super) fn step(
        &self,
        function: &Function,
        _module: &Module,
        activation: &mut Activation,
        runtime: &mut RuntimeState,
        frame_runtime: &mut FrameRuntimeState,
    ) -> Result<StepOutcome, InterpreterError> {
        let bytecode = function.bytecode();
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
            Opcode::Ldar => {
                let r = reg(&instr.operands, 0)?;
                let v = read_reg(activation, function, r)?;
                activation.set_accumulator(v);
                // M_JIT_C.2: track whether this PC's slot has only ever
                // been observed as int32. The JIT consumer drops the
                // `Ldar` tag guard once the feedback stabilises at
                // `Int32`; observing any non-int32 value promotes the
                // lattice to `Any`, which keeps the guard in place.
                let observation = if v.as_i32().is_some() {
                    ArithmeticFeedback::Int32
                } else {
                    ArithmeticFeedback::Any
                };
                frame_runtime.record_arithmetic(function, pc, observation);
            }
            Opcode::Star => {
                let r = reg(&instr.operands, 0)?;
                write_reg(activation, function, r, activation.accumulator())?;
            }
            Opcode::Mov => {
                let src = reg(&instr.operands, 0)?;
                let dst = reg(&instr.operands, 1)?;
                let v = read_reg(activation, function, src)?;
                write_reg(activation, function, dst, v)?;
            }
            Opcode::LdaSmi => {
                let imm = imm(&instr.operands, 0)?;
                activation.set_accumulator(RegisterValue::from_i32(imm));
            }
            Opcode::LdaUndefined => activation.set_accumulator(RegisterValue::undefined()),
            Opcode::LdaNull => activation.set_accumulator(RegisterValue::null()),
            Opcode::LdaTrue => activation.set_accumulator(RegisterValue::from_bool(true)),
            Opcode::LdaFalse => activation.set_accumulator(RegisterValue::from_bool(false)),
            Opcode::LdaTheHole => activation.set_accumulator(RegisterValue::hole()),
            Opcode::LdaNaN => {
                activation.set_accumulator(
                    RegisterValue::from_raw_bits(crate::value::TAG_NAN)
                        .expect("TAG_NAN is a valid RegisterValue bit pattern"),
                );
            }
            Opcode::LdaCurrentClosure => {
                if let Some(closure) = activation.closure_handle() {
                    activation.set_accumulator(RegisterValue::from_object_handle(closure.0));
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }
            Opcode::LdaNewTarget => {
                if let Some(nt) = activation.construct_new_target() {
                    activation.set_accumulator(RegisterValue::from_object_handle(nt.0));
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }
            Opcode::LdaConstStr => {
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
            Opcode::LdaConstF64 => {
                let idx = idx_operand(&instr.operands, 0)?;
                use crate::float::FloatId;
                let Some(value) = function.float_constants().get(FloatId(idx as u16)) else {
                    return Err(InterpreterError::NativeCall(Box::from(format!(
                        "v2 LdaConstF64: float id {idx} out of range"
                    ))));
                };
                activation.set_accumulator(
                    RegisterValue::from_raw_bits(value.to_bits())
                        .unwrap_or_else(RegisterValue::undefined),
                );
            }
            // M36: §6.1.6.2 BigInt literal — `42n` → heap BigInt.
            // Allocates a new BigInt object from the interned
            // decimal-string representation stored in the
            // function's side table.
            Opcode::LdaConstBigInt => {
                let idx = idx_operand(&instr.operands, 0)?;
                let Some(s) = function
                    .bigint_constants()
                    .get(crate::bigint::BigIntId(idx as u16))
                else {
                    return Err(InterpreterError::NativeCall(Box::from(format!(
                        "v2 LdaConstBigInt: bigint id {idx} out of range"
                    ))));
                };
                let handle = runtime.objects.alloc_bigint(s.to_string());
                // BigInt primitives use the dedicated `TAG_PTR_BIGINT` tag so
                // `is_bigint()` discriminators and `bigint_binary_op` decoders
                // both find the value; tagging as a regular object handle
                // would silently break `js_add`'s BigInt dispatch and trip
                // `property_lookup` into the `InvalidKind` branch.
                activation.set_accumulator(RegisterValue::from_bigint_handle(handle.0));
            }
            // M36: §22.2 RegExp literal — `/pattern/flags` creates
            // a RegExp object with `RegExp.prototype` as its
            // `[[Prototype]]`. Pattern + flags live in the
            // function's side table.
            Opcode::CreateRegExp => {
                let idx = idx_operand(&instr.operands, 0)?;
                let Some(entry) = function
                    .regexp_literals()
                    .get(crate::regexp::RegExpId(idx as u16))
                else {
                    return Err(InterpreterError::NativeCall(Box::from(format!(
                        "v2 CreateRegExp: regexp id {idx} out of range"
                    ))));
                };
                let pattern = entry.pattern.to_string();
                let flags = entry.flags.to_string();
                let prototype = runtime.intrinsics().regexp_prototype();
                let handle = runtime
                    .objects
                    .alloc_regexp(&pattern, &flags, Some(prototype));
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            Opcode::LdaThis => {
                // `this` lives in the receiver slot (hidden[0]).
                if let Some(slot) = function.frame_layout().receiver_slot() {
                    let v = activation.register(slot)?;
                    activation.set_accumulator(v);
                } else {
                    activation.set_accumulator(RegisterValue::undefined());
                }
            }
            // §12.2.6.8 CopyDataProperties — object-spread
            // implementation. Source value lives in the
            // accumulator; target object handle is the operand
            // register. Copies every own-enumerable data property
            // from source onto target, skipping symbols /
            // non-enumerable slots per spec. Runtime helper
            // already handles the full property-walk + excluded-
            // key matching.
            Opcode::CopyDataProperties => {
                let target_val = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let Some(target_handle) = target_val.as_object_handle() else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "CopyDataProperties target must be an object",
                    )));
                };
                let source = activation.accumulator();
                crate::property_copy::copy_data_properties(
                    runtime,
                    crate::object::ObjectHandle(target_handle),
                    source,
                    None,
                )
                .map_err(|err| match err {
                    crate::VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    crate::VmNativeCallError::Internal(msg) => InterpreterError::NativeCall(msg),
                })?;
            }
            // M35: §13.3.10 `import(expr)` — delegate to the
            // module-loader's thread-local-context
            // `dynamic_import_resolve`, which resolves + loads the
            // named module and returns a fulfilled Promise of its
            // namespace object. When called outside an
            // `execute_module_graph_shared` span the helper
            // returns `NativeCall("dynamic import: no module …
            // installed")`, which the normal error path surfaces
            // to user code as a thrown error.
            Opcode::DynamicImport => {
                let spec_reg = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let Some(spec_handle) = spec_reg.as_object_handle() else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "dynamic import: specifier must be a string",
                    )));
                };
                let spec_str = runtime
                    .objects
                    .string_value(crate::object::ObjectHandle(spec_handle))?
                    .map(|s| s.to_string())
                    .ok_or_else(|| {
                        InterpreterError::TypeError(Box::from(
                            "dynamic import: specifier must be a string",
                        ))
                    })?;
                let promise_value =
                    crate::module_loader::dynamic_import_resolve(&spec_str, runtime)?;
                activation.set_accumulator(promise_value);
            }
            // M35: `import.meta` — synthesise a fresh plain object
            // with the single `url` property (other fields, like
            // `import.meta.resolve`, land in later slices).
            Opcode::ImportMeta => {
                let meta = runtime.alloc_object();
                let url_prop = runtime.intern_property_name("url");
                let referrer = crate::module_loader::current_dynamic_import_referrer();
                let url_value = runtime.alloc_string(referrer.as_str());
                runtime.objects.set_property(
                    meta,
                    url_prop,
                    RegisterValue::from_object_handle(url_value.0),
                )?;
                activation.set_accumulator(RegisterValue::from_object_handle(meta.0));
            }

            // ---- Binary arithmetic (int32 fast path; generic bail later) ----
            //
            // Every successful int32 op records an
            // [`ArithmeticFeedback::Int32`] observation at this PC so
            // the JIT baseline can drop tag guards once the feedback
            // stabilises (M_JIT_C.2 trust-int32 elision). Slots are
            // attached sparsely by the source compiler, so recording a
            // non-existent slot is a no-op.
            Opcode::Add => {
                // M15: generic `+`. Int32 fast path stays on the
                // hot path (and records `Int32` feedback so
                // `M_JIT_C.2` trust-int32 elision continues to
                // apply); anything non-int32 falls through to
                // `RuntimeState::js_add`, which implements the
                // full §13.15.3 ApplyStringOrNumericBinaryOperator
                // sequence including string concatenation. Taking
                // the generic path forces `ArithmeticFeedback::Any`
                // so any later JIT recompile keeps the tag guard
                // and bailout pad intact.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                match acc.add_i32(rhs) {
                    Ok(sum) => {
                        activation.set_accumulator(sum);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                    }
                    Err(_) => {
                        let result = runtime.js_add(acc, rhs)?;
                        activation.set_accumulator(result);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                    }
                }
            }
            Opcode::Sub => {
                // §13.15.3 Subtraction. Int32 fast path records
                // `Int32` feedback; anything else falls through to
                // `js_subtract` (f64 / BigInt / ToNumeric).
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                match acc.sub_i32(rhs) {
                    Ok(diff) => {
                        activation.set_accumulator(diff);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                    }
                    Err(_) => {
                        let result = runtime.js_subtract(acc, rhs)?;
                        activation.set_accumulator(result);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                    }
                }
            }
            Opcode::Mul => {
                // §13.15.3 Multiplication. Int32 fast path first;
                // falls through to `js_multiply` for everything
                // else — non-i32 operands, overflow, BigInt, etc.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                match acc.mul_i32(rhs) {
                    Ok(prod) => {
                        activation.set_accumulator(prod);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                    }
                    Err(_) => {
                        let result = runtime.js_multiply(acc, rhs)?;
                        activation.set_accumulator(result);
                        frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                    }
                }
            }
            Opcode::BitwiseOr => {
                // §13.12.3 Bitwise operators — each operand goes
                // through ToInt32 (§7.1.6), which in turn calls
                // ToNumber. `as_i32()` is the fast path for pure
                // int32 operands; anything else — a heap Number,
                // Boolean object, string convertible to a number,
                // etc. — falls through to `js_to_int32` so the op
                // returns the spec-defined result instead of a
                // TypeError.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l | r));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::BitwiseAnd => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l & r));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::BitwiseXor => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l ^ r));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::Shl => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                // §13.9.2 — shift amount masked to low 5 bits.
                activation
                    .set_accumulator(RegisterValue::from_i32(l.wrapping_shl((r as u32) & 0x1F)));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::Shr => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                activation
                    .set_accumulator(RegisterValue::from_i32(l.wrapping_shr((r as u32) & 0x1F)));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::UShr => {
                // §13.9.3 — UShr coerces LHS via ToUint32 (i32 bit
                // pattern reused as u32).
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let (l, r, feedback) = coerce_int32_pair(runtime, acc, rhs)?;
                let l = l as u32;
                let r = r as u32;
                activation
                    .set_accumulator(RegisterValue::from_i32((l.wrapping_shr(r & 0x1F)) as i32));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::Div => {
                // §13.15.3 Division. Int32 fast-path gives truncated
                // division when it yields an i32 result; falls back
                // to `js_divide` (f64 / BigInt) for every other
                // input. `5 / 0` returns `Infinity` per spec — no
                // longer throws like the v1 int-only path did.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                if let (Some(l), Some(r)) = (acc.as_i32(), rhs.as_i32())
                    && r != 0
                    && let Some(q) = l.checked_div(r)
                    && q.checked_mul(r) == Some(l)
                {
                    activation.set_accumulator(RegisterValue::from_i32(q));
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                } else {
                    let result = runtime.js_divide(acc, rhs)?;
                    activation.set_accumulator(result);
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                }
            }
            Opcode::Mod => {
                // §13.15.3 Remainder. Same shape as Div.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                if let (Some(l), Some(r)) = (acc.as_i32(), rhs.as_i32())
                    && r != 0
                {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_rem(r)));
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                } else {
                    let result = runtime.js_remainder(acc, rhs)?;
                    activation.set_accumulator(result);
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                }
            }
            Opcode::Exp => {
                // §13.15.3 Exponentiation. No int32 fast-path — `2
                // ** 31` doesn't fit an i32 and small-result cases
                // still need NaN / Infinity handling, so always
                // delegate to `js_exponentiate`.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let acc = activation.accumulator();
                let result = runtime.js_exponentiate(acc, rhs)?;
                activation.set_accumulator(result);
                frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
            }

            // ---- Smi immediate variants ----
            //
            // Feedback recording mirrors the reg-form arithmetic above:
            // a successful Smi op necessarily saw an int32 acc, so we
            // record `ArithmeticFeedback::Int32` at each op's PC.
            Opcode::AddSmi => {
                // M15: AddSmi mirrors Add's generic fallback — when
                // acc isn't int32 (e.g. a string from `LdaConstStr`
                // feeding `"px:" + 5`), fall through to
                // `RuntimeState::js_add` against the immediate
                // materialised as an i32 RegisterValue, and demote
                // feedback to `Any`.
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                if let Some(l) = acc.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_add(v)));
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                } else {
                    let rhs = RegisterValue::from_i32(v);
                    let result = runtime.js_add(acc, rhs)?;
                    activation.set_accumulator(result);
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                }
            }
            Opcode::SubSmi => {
                // Mirror AddSmi: fall through to `js_subtract` when
                // acc isn't int32 so `obj - 2` on a Number object or
                // a string works per §13.15.3.
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                if let Some(l) = acc.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_sub(v)));
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                } else {
                    let rhs = RegisterValue::from_i32(v);
                    let result = runtime.js_subtract(acc, rhs)?;
                    activation.set_accumulator(result);
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                }
            }
            Opcode::MulSmi => {
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                if let Some(l) = acc.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_mul(v)));
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
                } else {
                    let rhs = RegisterValue::from_i32(v);
                    let result = runtime.js_multiply(acc, rhs)?;
                    activation.set_accumulator(result);
                    frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Any);
                }
            }
            Opcode::BitwiseOrSmi => {
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                let (l, feedback) = match acc.as_i32() {
                    Some(l) => (l, ArithmeticFeedback::Int32),
                    None => (runtime.js_to_int32(acc)?, ArithmeticFeedback::Any),
                };
                activation.set_accumulator(RegisterValue::from_i32(l | v));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::BitwiseAndSmi => {
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                let (l, feedback) = match acc.as_i32() {
                    Some(l) => (l, ArithmeticFeedback::Int32),
                    None => (runtime.js_to_int32(acc)?, ArithmeticFeedback::Any),
                };
                activation.set_accumulator(RegisterValue::from_i32(l & v));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::ShlSmi => {
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                let (l, feedback) = match acc.as_i32() {
                    Some(l) => (l, ArithmeticFeedback::Int32),
                    None => (runtime.js_to_int32(acc)?, ArithmeticFeedback::Any),
                };
                activation
                    .set_accumulator(RegisterValue::from_i32(l.wrapping_shl((v as u32) & 0x1F)));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }
            Opcode::ShrSmi => {
                let v = imm(&instr.operands, 0)?;
                let acc = activation.accumulator();
                let (l, feedback) = match acc.as_i32() {
                    Some(l) => (l, ArithmeticFeedback::Int32),
                    None => (runtime.js_to_int32(acc)?, ArithmeticFeedback::Any),
                };
                activation
                    .set_accumulator(RegisterValue::from_i32(l.wrapping_shr((v as u32) & 0x1F)));
                frame_runtime.record_arithmetic(function, pc, feedback);
            }

            // ---- Unary ops on accumulator ----
            Opcode::Inc => {
                // §13.4.4.1 / §13.4.5.1 ToNumeric before the +1.
                // `new Number(5)++`, `"3"++`, `true++` all need to
                // coerce to a number first rather than throw.
                let acc = activation.accumulator();
                if let Some(l) = acc.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_add(1)));
                } else {
                    let n = runtime.js_to_number(acc)?;
                    activation.set_accumulator(RegisterValue::from_number(n + 1.0));
                }
            }
            Opcode::Dec => {
                let acc = activation.accumulator();
                if let Some(l) = acc.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_sub(1)));
                } else {
                    let n = runtime.js_to_number(acc)?;
                    activation.set_accumulator(RegisterValue::from_number(n - 1.0));
                }
            }
            Opcode::Negate => {
                let value = activation.accumulator();
                if let Some(l) = value.as_i32() {
                    activation.set_accumulator(RegisterValue::from_i32(l.wrapping_neg()));
                } else {
                    let n = runtime.js_to_number(value)?;
                    activation.set_accumulator(RegisterValue::from_number(-n));
                }
            }
            Opcode::BitwiseNot => {
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(!l));
            }
            Opcode::LogicalNot => {
                // §7.1.2 ToBoolean — must be runtime-aware so empty
                // strings (falsy) and 0n BigInt (falsy) coerce right.
                // `Value::is_truthy()` can't reach heap values, so
                // routing through `js_to_boolean` is mandatory.
                let b = runtime.js_to_boolean(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_bool(!b));
            }
            Opcode::ToBoolean => {
                let b = runtime.js_to_boolean(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            Opcode::TypeOf => {
                let v = activation.accumulator();
                activation.set_accumulator(runtime.js_typeof(v)?);
            }

            // ---- Comparisons (int32 ordered) ----
            //
            // A successful int32 ordered compare saw int32 on both
            // sides; record Int32 so the JIT can elide the RHS tag
            // guard on trust-int32 recompile. `TestEqualStrict` is
            // polymorphic at the ISA level — we observe per-call and
            // let the monotonic lattice record whatever we actually
            // saw.
            Opcode::TestLessThan => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l < r));
                frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
            }
            Opcode::TestGreaterThan => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l > r));
                frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
            }
            Opcode::TestLessThanOrEqual => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l <= r));
                frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
            }
            Opcode::TestGreaterThanOrEqual => {
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l >= r));
                frame_runtime.record_arithmetic(function, pc, ArithmeticFeedback::Int32);
            }
            Opcode::TestEqualStrict => {
                // §7.2.16 IsStrictlyEqual. Raw bit comparison covers
                // the primitive cases (undefined/null/bool/int32/NaN
                // distinction is carried in the NaN-box tag), but
                // strings are heap objects and two distinct
                // `JsString` allocations with the same code-unit
                // sequence must compare equal. When both operands
                // are string handles, fall back to content compare
                // via `ObjectHeap::string_value`.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let lhs = activation.accumulator();
                let result = if lhs == rhs {
                    true
                } else if let (Some(lh), Some(rh)) =
                    (lhs.as_object_handle(), rhs.as_object_handle())
                {
                    let lh = crate::object::ObjectHandle(lh);
                    let rh = crate::object::ObjectHandle(rh);
                    let lstr = runtime.objects.string_value(lh).ok().flatten();
                    let rstr = runtime.objects.string_value(rh).ok().flatten();
                    match (lstr, rstr) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    }
                } else if let (Some(lb), Some(rb)) =
                    (lhs.as_bigint_handle(), rhs.as_bigint_handle())
                {
                    // §6.1.6.2.13 BigInt::equal — two distinct BigInt
                    // heap allocations for the same integer must compare
                    // strictly equal. Values are stored as decimal
                    // strings, so a byte-wise compare after canonical
                    // `to_string()` (no sign noise for 0) suffices.
                    let lh = crate::object::ObjectHandle(lb);
                    let rh = crate::object::ObjectHandle(rb);
                    let lstr = runtime.objects.bigint_value(lh).ok().flatten();
                    let rstr = runtime.objects.bigint_value(rh).ok().flatten();
                    match (lstr, rstr) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    }
                } else {
                    false
                };
                activation.set_accumulator(RegisterValue::from_bool(result));
                let observation = if lhs.as_i32().is_some() && rhs.as_i32().is_some() {
                    ArithmeticFeedback::Int32
                } else {
                    ArithmeticFeedback::Any
                };
                frame_runtime.record_arithmetic(function, pc, observation);
            }
            Opcode::TestEqual => {
                // Loose equality (§7.2.15 IsLooselyEqual). Delegate to
                // RuntimeState so number/string/object/BigInt coercions share
                // the same abstract-operation implementation as runtime APIs.
                let rhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let lhs = activation.accumulator();
                let result = runtime.js_loose_eq(lhs, rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(result));
            }
            Opcode::TestInstanceOf => {
                let lhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let rhs = activation.accumulator();
                let result = runtime.js_instance_of(lhs, rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(result));
            }
            Opcode::TestIn => {
                let lhs = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let rhs = activation.accumulator();
                let result = runtime.js_has_property(lhs, rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(result));
            }
            Opcode::TestNull => {
                let b = activation.accumulator() == RegisterValue::null();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            Opcode::TestUndefined => {
                let b = activation.accumulator() == RegisterValue::undefined();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }
            Opcode::TestUndetectable => {
                // §7.2.13: null / undefined / document.all-style values.
                // For now, equivalent to `null || undefined`.
                let v = activation.accumulator();
                let b = v == RegisterValue::null() || v == RegisterValue::undefined();
                activation.set_accumulator(RegisterValue::from_bool(b));
            }

            // ---- Jumps (byte-offset from end_pc) ----
            Opcode::Jump => {
                let off = jump_off(&instr.operands, 0)?;
                activation.set_pc(jump_target(next_pc, off));
                return Ok(StepOutcome::Continue);
            }
            Opcode::JumpIfToBooleanTrue => {
                let off = jump_off(&instr.operands, 0)?;
                // §7.1.2 ToBoolean — heap strings (""/non-empty) and
                // BigInt (0n/non-zero) must dispatch through the
                // runtime-aware helper, not `is_truthy()` which
                // defaults object-tagged values to truthy.
                if runtime.js_to_boolean(activation.accumulator())? {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfToBooleanFalse => {
                let off = jump_off(&instr.operands, 0)?;
                if !runtime.js_to_boolean(activation.accumulator())? {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfTrue => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator().as_bool() == Some(true) {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfFalse => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator().as_bool() == Some(false) {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfNull => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() == RegisterValue::null() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfNotNull => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() != RegisterValue::null() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfUndefined => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() == RegisterValue::undefined() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }
            Opcode::JumpIfNotUndefined => {
                let off = jump_off(&instr.operands, 0)?;
                if activation.accumulator() != RegisterValue::undefined() {
                    activation.set_pc(jump_target(next_pc, off));
                    return Ok(StepOutcome::Continue);
                }
            }

            // ---- Globals ----
            Opcode::LdaGlobal => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_property(function, runtime, prop_id)?;
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
            Opcode::StaGlobal => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_property(function, runtime, prop_id)?;
                let global_handle = runtime.intrinsics().global_object();
                let value = activation.accumulator();
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
            }
            Opcode::StaGlobalStrict => {
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_property(function, runtime, prop_id)?;
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
                runtime
                    .objects
                    .set_property(global_handle, property, value)?;
            }
            Opcode::TypeOfGlobal => {
                // `typeof foo` where `foo` is an unresolvable reference
                // must NOT throw — it returns "undefined". Walk the
                // global + call the existing typeof helper.
                let prop_id = idx_operand(&instr.operands, 0)?;
                let property = resolve_property(function, runtime, prop_id)?;
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
            Opcode::LdaUpvalue => {
                let idx = idx_operand(&instr.operands, 0)?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let cell = runtime.objects.closure_upvalue(closure, idx as usize)?;
                let value = runtime.objects.get_upvalue(cell)?;
                if value.is_hole() {
                    let err =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
                activation.set_accumulator(value);
            }
            Opcode::StaUpvalue => {
                let idx = idx_operand(&instr.operands, 0)?;
                let closure = activation
                    .closure_handle()
                    .ok_or(InterpreterError::MissingClosureContext)?;
                let cell = runtime.objects.closure_upvalue(closure, idx as usize)?;
                let value = activation.accumulator();
                runtime.objects.set_upvalue(cell, value)?;
            }

            // ---- Named property access ----
            //
            // All four property opcodes (named + keyed, read + write)
            // go through the runtime-level helpers rather than
            // `ObjectHeap::{get,set}_property` directly. The helpers
            // thread the live `PropertyNameRegistry` into the heap
            // lookup, which is necessary for Array / TypedArray /
            // String exotic paths that must materialise the
            // `PropertyNameId → name → canonical_array_index`
            // resolution — `get_property`/`set_property` use a default
            // (empty) registry and would silently miss array indices
            // and the array `length` property.
            Opcode::LdaNamedProperty => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let property = resolve_property(function, runtime, prop_id)?;
                // E1: auto-box primitive receivers so `(5n).toString`,
                // `(1).toFixed`, `"s".length` etc. walk the primitive's
                // prototype chain. `property_base_object_handle` returns
                // a wrapper for Boolean/Number/BigInt/Symbol and the
                // object itself for regular object handles — same path
                // `LdaKeyedProperty` already uses. Receiver stays as the
                // original primitive so accessor getters see the raw
                // value (spec §7.3.1 OrdinaryGet, receiver preserved).
                let handle = runtime.property_base_object_handle(target)?;
                // P1: probe the polymorphic inline cache before taking
                // the slow path. `PropertyFeedback` holds up to 4
                // observed `(shape_id, slot_index)` pairs per call
                // site; `get_shaped` returns the cached value in
                // O(1) when the object's current shape matches one
                // of the observed shapes. Any miss — accessor
                // property, prototype lookup, different shape —
                // falls through to `ordinary_get` below. The
                // polymorphic probe is just a small linear scan on
                // shape_id, and the common case is hot loops
                // touching one or two shapes on the same PC.
                let cached_value = if let Some(fb) =
                    property_feedback_for_pc(function, &frame_runtime.feedback_vector, pc)
                {
                    probe_property_inline_cache(fb, &runtime.objects, handle)?
                } else {
                    None
                };
                let value = if let Some(v) = cached_value {
                    v
                } else {
                    // M29: use `ordinary_get` so accessor getters are
                    // invoked with the target as receiver. Data and
                    // "not found" cases fall through unchanged. The
                    // resolved lookup doubles as the observation
                    // source for the P1 inline-cache state machine.
                    let lookup = runtime.property_lookup(handle, property).map_err(|err| {
                        InterpreterError::NativeCall(format!("property_lookup: {err:?}").into())
                    })?;
                    match lookup {
                        Some(l) => {
                            // P1: observe the shape + slot offset
                            // when the lookup hit a data property
                            // on the direct object (owner ==
                            // handle). Prototype hits don't
                            // populate the IC — their shape isn't
                            // the one we'd guard on next time.
                            if let Some(cache) = l.cache()
                                && l.owner() == handle
                            {
                                frame_runtime.record_property(
                                    function,
                                    pc,
                                    cache.shape_id(),
                                    cache.slot_index(),
                                );
                            }
                            match l.value() {
                                crate::object::PropertyValue::Data { value: v, .. } => v,
                                crate::object::PropertyValue::Accessor { getter, .. } => runtime
                                    .call_callable_for_accessor(getter, target, &[])
                                    .map_err(|err| match err {
                                        InterpreterError::UncaughtThrow(v) => {
                                            InterpreterError::UncaughtThrow(v)
                                        }
                                        other => other,
                                    })?,
                            }
                        }
                        None => RegisterValue::undefined(),
                    }
                };
                activation.set_accumulator(value);
            }
            // §13.5.1 `delete obj.prop` / `delete obj[key]` —
            // delegates to `ObjectHeap::delete_property_with_registry`
            // which handles array-length, non-configurable, and
            // prototype-chain edges per §9.1.10 [[Delete]]. Result
            // (boolean: whether deletion succeeded) lands in the
            // accumulator.
            Opcode::DelNamedProperty => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let property = resolve_property(function, runtime, prop_id)?;
                let Some(handle) = target.as_object_handle() else {
                    activation.set_accumulator(RegisterValue::from_bool(true));
                    return Ok(StepOutcome::Continue);
                };
                // Split borrow on the runtime struct: `objects`
                // mutably, `property_names` immutably — both live
                // as separate fields so Rust disjoint-borrow rules
                // let us use them side-by-side here.
                let result = runtime
                    .objects
                    .delete_property_with_registry(
                        crate::object::ObjectHandle(handle),
                        property,
                        &runtime.property_names,
                    )
                    .unwrap_or(false);
                activation.set_accumulator(RegisterValue::from_bool(result));
            }
            Opcode::DelKeyedProperty => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let key = activation.accumulator();
                let Some(handle) = target.as_object_handle() else {
                    activation.set_accumulator(RegisterValue::from_bool(true));
                    return Ok(StepOutcome::Continue);
                };
                let property = key_to_property_name(runtime, key)?;
                // Split borrow on the runtime struct: `objects`
                // mutably, `property_names` immutably — both live
                // as separate fields so Rust disjoint-borrow rules
                // let us use them side-by-side here.
                let result = runtime
                    .objects
                    .delete_property_with_registry(
                        crate::object::ObjectHandle(handle),
                        property,
                        &runtime.property_names,
                    )
                    .unwrap_or(false);
                activation.set_accumulator(RegisterValue::from_bool(result));
            }
            Opcode::StaNamedProperty => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let property = resolve_property(function, runtime, prop_id)?;
                let Some(handle) = target.as_object_handle() else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "v2 StaNamedProperty: receiver is not an object",
                    )));
                };
                let value = activation.accumulator();
                // M29: use `ordinary_set` so accessor setters fire
                // when the target is part of an accessor chain.
                // Data-property paths still share
                // `set_named_property`'s cache-aware storage via
                // `ordinary_set`'s `ordinary_set_on_receiver`.
                runtime
                    .ordinary_set(crate::object::ObjectHandle(handle), property, target, value)
                    .map_err(|err| match err {
                        crate::VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                        crate::VmNativeCallError::Internal(msg) => {
                            InterpreterError::NativeCall(msg)
                        }
                    })?;
            }

            // ---- Keyed property access ----
            //
            // v2 convention: `LdaKeyedProperty r` reads the key from the
            // accumulator and the base object from register `r`, writing
            // the fetched value back into the accumulator.
            Opcode::LdaKeyedProperty => {
                let base = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let key = activation.accumulator();
                let handle = runtime.property_base_object_handle(base)?;
                let prop = key_to_property_name(runtime, key)?;
                let receiver = RegisterValue::from_object_handle(handle.0);
                let value =
                    runtime
                        .ordinary_get(handle, prop, receiver)
                        .map_err(|err| match err {
                            crate::VmNativeCallError::Thrown(v) => {
                                InterpreterError::UncaughtThrow(v)
                            }
                            crate::VmNativeCallError::Internal(msg) => {
                                InterpreterError::NativeCall(msg)
                            }
                        })?;
                activation.set_accumulator(value);
            }
            Opcode::StaKeyedProperty => {
                // v2: `StaKeyedProperty r0 r1`: r0[r1] = acc.
                // Uses `set_named_property` (not `ordinary_set`) so
                // array indexed-storage + `length` stay on the
                // registry-aware path. Accessor setters inherited
                // via the prototype chain are rare for keyed
                // access; M29 handles them via `StaNamedProperty`
                // instead.
                let base = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let key = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let value = activation.accumulator();
                let handle = runtime.property_set_target_handle(base)?;
                let prop = key_to_property_name(runtime, key)?;
                runtime.set_named_property(handle, prop, value)?;
            }

            // ---- Object / array allocation ----
            Opcode::CreateObject => {
                let handle = runtime.alloc_object();
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            Opcode::CreateArray => {
                let handle = runtime.alloc_array();
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            // §10.4.4 — Rest parameter. Collects `overflow_args`
            // (arguments beyond the formal non-rest param count)
            // into a fresh Array and writes the handle to the
            // accumulator. The compiler emits this at function
            // entry for `function f(..., ...rest)`; the trailing
            // `Star r_rest` binds it to the rest local.
            Opcode::CreateRestParameters => {
                let arr = runtime.alloc_array();
                let overflow = std::mem::take(&mut activation.overflow_args);
                for value in overflow {
                    // `push_element` bumps length + writes into
                    // the dense elements slot.
                    if runtime.objects.push_element(arr, value).is_err() {
                        let err =
                            runtime.alloc_type_error("CreateRestParameters: array push failed")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                }
                activation.set_accumulator(RegisterValue::from_object_handle(arr.0));
            }

            // `CreateClosure idx, flags` — M25. Builds a fresh
            // closure object bound to the `ClosureTemplate`
            // registered at this PC in the current function's
            // `ClosureTable`. Each capture descriptor resolves to
            // an upvalue handle:
            // - `CaptureDescriptor::Register(reg)` promotes the
            //   current frame's register into an open upvalue
            //   cell — so later writes to the same slot sync
            //   through the cell and the inner closure observes
            //   live values.
            // - `CaptureDescriptor::Upvalue(id)` re-captures an
            //   existing upvalue the current closure already
            //   holds, letting grand-closures reach past one
            //   level of nesting.
            //
            // The `Idx` operand is the callee's function index in
            // the current module; the `Imm` carries flags that
            // future milestones (generator, async) will use. For
            // M25 we ignore the operand flags and rely on
            // `ClosureTemplate::flags()` so the metadata stays in
            // one place.
            Opcode::CreateClosure => {
                use crate::closure::CaptureDescriptor;
                let callee_idx_raw = idx_operand(&instr.operands, 0)?;
                let _flags_imm = imm(&instr.operands, 1)?;
                let template = function.closures().get(pc).ok_or_else(|| {
                    InterpreterError::NativeCall(Box::from(
                        "CreateClosure: no ClosureTemplate for this PC",
                    ))
                })?;
                if template.callee().0 != callee_idx_raw {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "CreateClosure: template callee mismatches opcode operand",
                    )));
                }
                let mut upvalues: Vec<crate::object::ObjectHandle> =
                    Vec::with_capacity(template.captures().len());
                for cap in template.captures() {
                    let handle = match cap {
                        CaptureDescriptor::Register(reg) => {
                            activation.capture_bytecode_register_upvalue(function, runtime, *reg)?
                        }
                        CaptureDescriptor::Upvalue(id) => {
                            let parent = activation
                                .closure_handle()
                                .ok_or(InterpreterError::MissingClosureContext)?;
                            runtime.objects.closure_upvalue(parent, id.0 as usize)?
                        }
                    };
                    upvalues.push(handle);
                }
                let handle = runtime.alloc_closure(template.callee(), upvalues, template.flags());
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }

            // ---- Coercions reusing runtime helpers ----
            Opcode::ToNumber => {
                let v = activation.accumulator();
                let n = runtime.js_to_number(v)?;
                activation.set_accumulator(RegisterValue::from_number(n));
            }
            Opcode::ToString => {
                let v = activation.accumulator();
                let text = runtime.js_to_string(v)?;
                let handle = runtime.alloc_string(text.into_string());
                activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
            }
            Opcode::ToPropertyKey => {
                // §7.1.19 ToPropertyKey — keep Symbols as-is, coerce
                // everything else via ToPrimitive(hint=String) + ToString.
                let v = activation.accumulator();
                let primitive =
                    runtime.js_to_primitive_with_hint(v, super::ToPrimitiveHint::String)?;
                if primitive.as_symbol_id().is_some() {
                    activation.set_accumulator(primitive);
                } else {
                    let text = runtime.js_to_string(primitive)?;
                    let handle = runtime.alloc_string(text.into_string());
                    activation.set_accumulator(RegisterValue::from_object_handle(handle.0));
                }
            }

            // ---- Asserts / TDZ / class guards ----
            Opcode::AssertNotHole => {
                if activation.accumulator().is_hole() {
                    let err =
                        runtime.alloc_reference_error("Cannot access uninitialized binding")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }
            Opcode::ThrowConstAssign => {
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
            Opcode::CallUndefinedReceiver => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let (base, count) = reg_list(&instr.operands, 1)?;
                let args = read_reg_list(activation, function, base, count)?;
                let Some(handle) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match self.call_callable_bytecode(
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
            Opcode::CallAnyReceiver | Opcode::CallProperty => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let receiver = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function, base, count)?;
                let Some(handle) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match self.call_callable_bytecode(
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
            Opcode::CallDirect => {
                let fn_index_raw = idx_operand(&instr.operands, 0)?;
                let (base, count) = reg_list(&instr.operands, 1)?;
                let args = read_reg_list(activation, function, base, count)?;
                let callee_idx = crate::module::FunctionIndex(fn_index_raw);
                match self.call_direct_bytecode(runtime, _module, callee_idx, &args) {
                    Ok(value) => {
                        activation.refresh_open_upvalues_from_cells(runtime)?;
                        activation.set_accumulator(value);
                    }
                    Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                    Err(other) => return Ok(other),
                }
            }
            // `CallSpread r_callee, r_receiver, RegList { base: r_args, count: 1 }`
            // — method-style call whose argument list is a single
            // Array held in the register pointed to by the RegList.
            // The interpreter unpacks the array's elements into
            // individual arguments before dispatching through
            // `call_callable_bytecode`. The compiler builds the
            // args array inline (see `lower_call_with_spread`)
            // using `ArrayPush` + `SpreadIntoArray`, so every
            // spread is flattened by the time the Array reaches
            // this opcode.
            Opcode::CallSpread => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let receiver = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                if count != 1 {
                    let err = runtime
                        .alloc_type_error("CallSpread expects a single args-array operand")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
                let base_idx = RegisterIndex::try_from(base).map_err(|_| {
                    InterpreterError::NativeCall("CallSpread args register overflow".into())
                })?;
                let args_val = read_reg(activation, function, base_idx)?;
                let Some(args_handle) =
                    args_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err =
                        runtime.alloc_type_error("CallSpread args must be an Array object")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let len = match runtime.objects.array_length(args_handle) {
                    Ok(Some(n)) => n,
                    _ => {
                        let err =
                            runtime.alloc_type_error("CallSpread args must be an Array object")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                };
                let mut args: Vec<RegisterValue> = Vec::with_capacity(len);
                for i in 0..len {
                    let v = runtime
                        .objects
                        .get_index(args_handle, i)
                        .ok()
                        .flatten()
                        .unwrap_or_else(RegisterValue::undefined);
                    args.push(v);
                }
                let Some(callable) = target.as_object_handle() else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                match self.call_callable_bytecode(
                    runtime,
                    crate::object::ObjectHandle(callable),
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
            Opcode::TailCall => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let receiver = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function, base, count)?;
                let Some(callable) = target.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("Value is not callable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };

                let is_plain_closure =
                    matches!(
                        runtime.objects.kind(callable),
                        Ok(crate::object::HeapValueKind::Closure)
                    ) && !runtime.objects.closure_flags(callable).is_ok_and(|f| {
                        f.is_generator() || f.is_async() || f.is_class_constructor()
                    }) && runtime.objects.host_function(callable)?.is_none();

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
                        callee_activation.overflow_args = args[param_count as usize..].to_vec();
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
                    match self.call_callable_bytecode(runtime, callable, receiver, &args) {
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
            // §13.3.5 `new C(...args)` — identical to `Construct`
            // but the arg-window points at a single Array operand
            // the compiler built from the spread + plain args.
            Opcode::ConstructSpread => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let new_target = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                if count != 1 {
                    let err = runtime
                        .alloc_type_error("ConstructSpread expects a single args-array operand")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
                let base_idx = RegisterIndex::try_from(base).map_err(|_| {
                    InterpreterError::NativeCall("ConstructSpread args register overflow".into())
                })?;
                let args_val = read_reg(activation, function, base_idx)?;
                let Some(args_handle) =
                    args_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err =
                        runtime.alloc_type_error("ConstructSpread args must be an Array object")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let len = match runtime.objects.array_length(args_handle) {
                    Ok(Some(n)) => n,
                    _ => {
                        let err = runtime
                            .alloc_type_error("ConstructSpread args must be an Array object")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                };
                let mut args: Vec<RegisterValue> = Vec::with_capacity(len);
                for i in 0..len {
                    let v = runtime
                        .objects
                        .get_index(args_handle, i)
                        .ok()
                        .flatten()
                        .unwrap_or_else(RegisterValue::undefined);
                    args.push(v);
                }
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
            Opcode::Construct => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let new_target = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let (base, count) = reg_list(&instr.operands, 2)?;
                let args = read_reg_list(activation, function, base, count)?;
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
            Opcode::AssertConstructor => {
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
            // `GetIterator r` — §7.4.1 GetIterator(obj, sync).
            // Delegates to `RuntimeState::iterator_open`, which
            // looks up `@@iterator` on `target`, invokes it with
            // `target` as receiver, and validates the returned
            // iterator is an Object. Built-in iterables
            // (Array / String / Map / Set / TypedArray) install
            // their `@@iterator` slot during intrinsic bootstrap,
            // so built-in and user-defined iterables share the
            // same dispatch path.
            Opcode::GetIterator => {
                let target = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                match runtime.iterator_open(target) {
                    Ok(iter) => {
                        activation.set_accumulator(RegisterValue::from_object_handle(iter.0));
                    }
                    Err(crate::VmNativeCallError::Thrown(value)) => {
                        return Ok(StepOutcome::Throw(value));
                    }
                    Err(crate::VmNativeCallError::Internal(msg)) => {
                        return Err(InterpreterError::NativeCall(msg));
                    }
                }
            }

            // `IteratorNext r` — step the iterator. Writes `value` into
            // the accumulator and the `done` flag into
            // `activation.secondary_result`. The compiler-emitted
            // sequence `IteratorNext r; Star r_value; <branch on
            // secondary_result>` preserves both channels.
            Opcode::IteratorNext => {
                let iter_reg = reg(&instr.operands, 0)?;
                let iter_val = read_reg(activation, function, iter_reg)?;
                let Some(iterator) = iter_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("IteratorNext target is not an object")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let step = runtime.iterator_next(iterator)?;
                activation.set_accumulator(step.value());
                activation.set_secondary_result(RegisterValue::from_bool(step.is_done()));
            }

            // §7.4.2 IteratorStep + §14.7.5 for-of driver —
            // `IteratorStep value_reg iter_reg`. Calls
            // `iterator.next()` via `iterator_step_protocol` so
            // user-defined iterators that expose a `next()`
            // method go through the full Object-shaped-result
            // path (ToBoolean(result.done), Get(result.value))
            // rather than the built-in-only fast path.
            Opcode::IteratorStep => {
                let value_dst = reg(&instr.operands, 0)?;
                let iter_val = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let Some(iter) = iter_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("IteratorStep target is not an iterator")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let step = match runtime.iterator_step_protocol(iter) {
                    Ok(step) => step,
                    Err(crate::VmNativeCallError::Thrown(value)) => {
                        return Ok(StepOutcome::Throw(value));
                    }
                    Err(crate::VmNativeCallError::Internal(msg)) => {
                        return Err(InterpreterError::NativeCall(msg));
                    }
                };
                if step.is_done() {
                    activation.set_accumulator(RegisterValue::from_bool(true));
                } else {
                    write_reg(activation, function, value_dst, step.value())?;
                    activation.set_accumulator(RegisterValue::from_bool(false));
                }
            }

            // `IteratorClose r` — side-effectful; closes built-in
            // iterators and is a no-op for non-built-ins. Does not
            // write the accumulator (Phase 3b.9b will wire the
            // `.return()` protocol for custom iterators).
            Opcode::IteratorClose => {
                let iter_val = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                if let Some(h) = iter_val.as_object_handle() {
                    let _ = runtime
                        .objects
                        .iterator_close(crate::object::ObjectHandle(h));
                }
            }

            // `ForInEnumerate r` — allocates a for-in property-key
            // iterator over `r` and its prototype chain. Writes the
            // iterator handle into the accumulator. `null` / `undefined`
            // source objects route to an empty iterator per §14.7.5.6
            // step 6 ("if expr is null or undefined then return break").
            Opcode::ForInEnumerate => {
                let src = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let iterator = match src.as_object_handle() {
                    Some(handle) => {
                        runtime.alloc_property_iterator(crate::object::ObjectHandle(handle))?
                    }
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
            Opcode::ForInNext => {
                let value_dst = reg(&instr.operands, 0)?;
                let iter_val = read_reg(activation, function, reg(&instr.operands, 1)?)?;
                let Some(iter) = iter_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err =
                        runtime.alloc_type_error("ForInNext target is not a property iterator")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let step = runtime.objects.property_iterator_next(iter)?;
                if step.is_done() {
                    activation.set_accumulator(RegisterValue::from_bool(true));
                } else {
                    write_reg(activation, function, value_dst, step.value())?;
                    activation.set_accumulator(RegisterValue::from_bool(false));
                }
            }

            // `ArrayPush r` — `r.push(acc)`. r must be an ordinary
            // Array object. Used by spread-emitting code. Failures
            // (not-an-array) surface as a catchable TypeError.
            Opcode::ArrayPush => {
                let arr_val = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let value = activation.accumulator();
                let Some(arr) = arr_val.as_object_handle().map(crate::object::ObjectHandle) else {
                    let err = runtime.alloc_type_error("ArrayPush target is not an array")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                // `push_element` handles Array-kind validation and the
                // extensible / length-writable / elements-writable flags;
                // a non-Array arg surfaces as a TypeError for the user.
                if runtime.objects.push_element(arr, value).is_err() {
                    let err = runtime.alloc_type_error("ArrayPush target is not an array")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // `SpreadIntoArray r_arr` — iterates the value in acc
            // via the iterator protocol and pushes each produced
            // value into `r_arr` (an Array). Used by the compiler
            // to expand `...iterable` inside an array literal or
            // while building a CallSpread argument array.
            //
            // §13.2.4 ArrayAccumulation — spread element case.
            Opcode::SpreadIntoArray => {
                let arr_val = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let Some(arr) = arr_val.as_object_handle().map(crate::object::ObjectHandle) else {
                    let err = runtime.alloc_type_error("SpreadIntoArray target is not an array")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let source = activation.accumulator();
                let Some(source_handle) =
                    source.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("Spread argument is not iterable")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let iter = match runtime.objects.alloc_iterator(source_handle) {
                    Ok(h) => h,
                    Err(_) => {
                        let err = runtime.alloc_type_error("Spread argument is not iterable")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                };
                loop {
                    let step = runtime.iterator_next(iter)?;
                    if step.is_done() {
                        break;
                    }
                    if runtime.objects.push_element(arr, step.value()).is_err() {
                        let err = runtime.alloc_type_error("SpreadIntoArray push failed")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                }
                // Leave acc untouched — callers that need the array
                // read it from the register.
            }

            // ---- Control ----
            Opcode::Return => {
                return Ok(StepOutcome::Return(activation.accumulator()));
            }
            Opcode::Throw => {
                return Ok(StepOutcome::Throw(activation.accumulator()));
            }
            Opcode::SetPendingReturn => {
                activation.set_pending_abrupt_completion(PendingAbruptCompletion::Return(
                    activation.accumulator(),
                ));
            }
            Opcode::SetPendingJump => {
                let target = imm(&instr.operands, 0)?;
                let target = u32::try_from(target).map_err(|_| {
                    InterpreterError::NativeCall(Box::from(
                        "SetPendingJump target pc must be non-negative",
                    ))
                })?;
                activation.set_pending_abrupt_completion(PendingAbruptCompletion::Jump(target));
            }
            Opcode::PushPendingFinally => {
                let target = imm(&instr.operands, 0)?;
                let target = u32::try_from(target).map_err(|_| {
                    InterpreterError::NativeCall(Box::from(
                        "PushPendingFinally target pc must be non-negative",
                    ))
                })?;
                activation.push_pending_finally(target);
            }
            Opcode::ResumeAbrupt => {
                if let Some(target_pc) = activation.pop_pending_finally() {
                    activation.set_pc(target_pc);
                    return Ok(StepOutcome::Continue);
                }
                if let Some(completion) = activation.take_pending_abrupt_completion() {
                    match completion {
                        PendingAbruptCompletion::Return(value) => {
                            return Ok(StepOutcome::Return(value));
                        }
                        PendingAbruptCompletion::Jump(target_pc) => {
                            activation.set_pc(target_pc);
                            return Ok(StepOutcome::Continue);
                        }
                        PendingAbruptCompletion::Throw(value) => {
                            return Ok(StepOutcome::Throw(value));
                        }
                    }
                }
            }
            Opcode::PushUsingScope => {
                activation.push_using_scope();
            }
            Opcode::AddDisposableResource => {
                let value = read_reg(activation, function, reg(&instr.operands, 0)?)?;
                let await_dispose = imm(&instr.operands, 1)? != 0;
                match add_disposable_resource(runtime, activation, value, await_dispose) {
                    Ok(()) => {}
                    Err(value) => return Ok(StepOutcome::Throw(value)),
                }
            }
            Opcode::DisposeUsingScope => {
                dispose_using_scope(self, runtime, _module, activation)?;
            }
            // §14.14 ThrowStatement + §14.15.3 TryStatement. The
            // dispatcher's main loop (see `run_completion_with_runtime`)
            // already handles the transfer: on
            // `StepOutcome::Throw(value)`, it calls
            // `transfer_exception`, which consults the function's
            // `ExceptionTable`, stashes the value on
            // `activation.pending_exception`, and jumps to the
            // handler PC. `LdaException` is the handler's first
            // opcode — it moves that pending value into acc so
            // catch parameters and `throw e;` rethrows observe it.
            // `ReThrow` re-raises the pending exception without
            // requiring the program to explicitly `throw acc` — used
            // by the finally-after-throw path.
            Opcode::LdaException => {
                let value = activation
                    .take_pending_exception()
                    .unwrap_or_else(RegisterValue::undefined);
                activation.set_accumulator(value);
            }
            Opcode::ReThrow => {
                let value = activation
                    .take_pending_exception()
                    .unwrap_or_else(|| activation.accumulator());
                return Ok(StepOutcome::Throw(value));
            }

            // §10.2.5 MakeMethod — stamp `[[HomeObject]]` onto the
            // closure held in `r_closure`. Emitted by the class body
            // lowering for both methods and the constructor so that
            // `super.x` references from inside the function's body
            // resolve against `home_object.[[GetPrototypeOf]]()`.
            Opcode::SetHomeObject => {
                let closure_reg = reg(&instr.operands, 0)?;
                let home_reg = reg(&instr.operands, 1)?;
                let closure_val = read_reg(activation, function, closure_reg)?;
                let home_val = read_reg(activation, function, home_reg)?;
                let Some(closure_handle) = closure_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "SetHomeObject: target is not a closure",
                    )));
                };
                let Some(home_handle) =
                    home_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "SetHomeObject: home object is not an object",
                    )));
                };
                runtime
                    .objects
                    .set_closure_home_object(closure_handle, home_handle)
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!("SetHomeObject: {err:?}")))
                    })?;
            }

            // §13.3.7 SuperProperty read — resolves `super.name` against
            // `activeFunction.[[HomeObject]].[[GetPrototypeOf]]()` with
            // `thisValue` (carried in `r_receiver`) preserved as the
            // `[[Get]]` receiver so accessor getters see the correct
            // `this` binding.
            Opcode::GetSuperProperty => {
                let receiver_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let receiver = read_reg(activation, function, receiver_reg)?;
                let base = super_property_base(activation, runtime)?;
                let property = resolve_property(function, runtime, prop_id)?;
                let value = runtime
                    .ordinary_get(base, property, receiver)
                    .map_err(|error| match error {
                        crate::VmNativeCallError::Thrown(value) => {
                            InterpreterError::UncaughtThrow(value)
                        }
                        crate::VmNativeCallError::Internal(msg) => {
                            InterpreterError::NativeCall(msg)
                        }
                    })?;
                activation.set_accumulator(value);
            }

            // §13.3.7 SuperProperty read with a computed key — mirrors
            // `GetSuperProperty` but the key register is evaluated via
            // `ToPropertyKey` before the lookup.
            Opcode::GetSuperPropertyComputed => {
                let receiver_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let receiver = read_reg(activation, function, receiver_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let base = super_property_base(activation, runtime)?;
                let property = key_to_property_name(runtime, key)?;
                let value = runtime
                    .ordinary_get(base, property, receiver)
                    .map_err(|error| match error {
                        crate::VmNativeCallError::Thrown(value) => {
                            InterpreterError::UncaughtThrow(value)
                        }
                        crate::VmNativeCallError::Internal(msg) => {
                            InterpreterError::NativeCall(msg)
                        }
                    })?;
                activation.set_accumulator(value);
            }

            // §13.3.7 SuperProperty write — performs `[[Set]]` on
            // `activeFunction.[[HomeObject]].[[GetPrototypeOf]]()` with
            // the receiver supplied in `r_receiver` and the value
            // carried in the accumulator. Strict-mode `[[Set]]` failures
            // surface as a TypeError.
            Opcode::SetSuperProperty => {
                let receiver_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let receiver = read_reg(activation, function, receiver_reg)?;
                let value = activation.accumulator();
                let base = super_property_base(activation, runtime)?;
                let property = resolve_property(function, runtime, prop_id)?;
                let ok = runtime
                    .ordinary_set(base, property, receiver, value)
                    .map_err(|error| match error {
                        crate::VmNativeCallError::Thrown(value) => {
                            InterpreterError::UncaughtThrow(value)
                        }
                        crate::VmNativeCallError::Internal(msg) => {
                            InterpreterError::NativeCall(msg)
                        }
                    })?;
                if !ok {
                    let err =
                        runtime.alloc_type_error("Cannot assign to read-only super property")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // §13.3.7 SuperProperty write with a computed key.
            Opcode::SetSuperPropertyComputed => {
                let receiver_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let receiver = read_reg(activation, function, receiver_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let value = activation.accumulator();
                let base = super_property_base(activation, runtime)?;
                let property = key_to_property_name(runtime, key)?;
                let ok = runtime
                    .ordinary_set(base, property, receiver, value)
                    .map_err(|error| match error {
                        crate::VmNativeCallError::Thrown(value) => {
                            InterpreterError::UncaughtThrow(value)
                        }
                        crate::VmNativeCallError::Internal(msg) => {
                            InterpreterError::NativeCall(msg)
                        }
                    })?;
                if !ok {
                    let err =
                        runtime.alloc_type_error("Cannot assign to read-only super property")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // §13.3.7.1 SuperCall with spread — `super(...args)`
            // inside a derived constructor. `RegList` carries a
            // single register holding an Array whose elements are
            // flattened into the super() argument list.
            Opcode::CallSuperSpread => {
                let (base, count) = reg_list(&instr.operands, 0)?;
                if count != 1 {
                    let err = runtime
                        .alloc_type_error("CallSuperSpread expects a single args-array operand")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
                let base_idx = RegisterIndex::try_from(base).map_err(|_| {
                    InterpreterError::NativeCall(Box::from(
                        "CallSuperSpread args register overflow",
                    ))
                })?;
                let args_val = read_reg(activation, function, base_idx)?;
                let Some(args_handle) =
                    args_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err =
                        runtime.alloc_type_error("CallSuperSpread args must be an Array object")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let len = match runtime.objects.array_length(args_handle) {
                    Ok(Some(n)) => n,
                    _ => {
                        let err = runtime
                            .alloc_type_error("CallSuperSpread args must be an Array object")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                };
                let mut args: Vec<RegisterValue> = Vec::with_capacity(len);
                for i in 0..len {
                    let v = runtime
                        .objects
                        .get_index(args_handle, i)
                        .ok()
                        .flatten()
                        .unwrap_or_else(RegisterValue::undefined);
                    args.push(v);
                }
                match super_call_dispatch(activation, function, runtime, &args)? {
                    SuperCallOutcome::Continue => {}
                    SuperCallOutcome::Throw(v) => return Ok(StepOutcome::Throw(v)),
                }
            }

            // §13.3.7.1 SuperCall — `super(args)` inside a derived
            // constructor. Resolves the super constructor from the
            // active closure's `[[GetPrototypeOf]]()`, forwards the
            // current frame's `[[NewTarget]]`, and initializes the
            // derived-constructor `this` binding with the freshly
            // constructed receiver.
            Opcode::CallSuper => {
                let (base, count) = reg_list(&instr.operands, 0)?;
                let args = read_reg_list(activation, function, base, count)?;
                match super_call_dispatch(activation, function, runtime, &args)? {
                    SuperCallOutcome::Continue => {}
                    SuperCallOutcome::Throw(v) => return Ok(StepOutcome::Throw(v)),
                }
            }

            // §15.7.14 ClassDefinitionEvaluation steps 6-7 — wire the
            // derived class's heritage: `Sub.__proto__ = Super` and
            // `Sub.prototype.__proto__ = Super.prototype`. Rejects
            // non-null, non-constructor supers with a TypeError to
            // match step 5.c.
            Opcode::SetClassHeritage => {
                let class_reg = reg(&instr.operands, 0)?;
                let super_reg = reg(&instr.operands, 1)?;
                let class_val = read_reg(activation, function, class_reg)?;
                let super_val = read_reg(activation, function, super_reg)?;
                let Some(class_handle) = class_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "SetClassHeritage: class is not an object",
                    )));
                };
                // Resolve the class's own prototype object (its
                // `.prototype` data property, seeded by
                // `alloc_closure`).
                let prototype_name = runtime.intern_property_name("prototype");
                let class_prototype = match runtime.property_lookup(class_handle, prototype_name)? {
                    Some(lookup) => match lookup.value() {
                        crate::object::PropertyValue::Data { value, .. } => value,
                        crate::object::PropertyValue::Accessor { .. } => RegisterValue::undefined(),
                    },
                    None => RegisterValue::undefined(),
                };
                let Some(class_prototype_handle) = class_prototype
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "SetClassHeritage: class prototype slot is missing",
                    )));
                };

                // Step 5: classify the superclass expression value.
                if super_val.is_null() {
                    // `class Sub extends null {}` — constructorParent
                    // stays %Function.prototype% (already the default);
                    // protoParent becomes null.
                    runtime
                        .objects
                        .set_prototype(class_prototype_handle, None)
                        .map_err(|err| {
                            InterpreterError::NativeCall(Box::from(format!(
                                "SetClassHeritage: clear proto parent failed: {err:?}"
                            )))
                        })?;
                } else {
                    let Some(super_handle) = super_val
                        .as_object_handle()
                        .map(crate::object::ObjectHandle)
                    else {
                        let err = runtime
                            .alloc_type_error("Class extends value is not a constructor or null")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    };
                    if !runtime.is_constructible(super_handle) {
                        let err = runtime
                            .alloc_type_error("Class extends value is not a constructor or null")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                    // `Sub.__proto__ = Super` — static method chain.
                    runtime
                        .objects
                        .set_prototype(class_handle, Some(super_handle))
                        .map_err(|err| {
                            InterpreterError::NativeCall(Box::from(format!(
                                "SetClassHeritage: static chain: {err:?}"
                            )))
                        })?;
                    // Read `Super.prototype`, rejecting non-Object +
                    // non-null per step 5.d.ii.
                    let super_proto = match runtime.property_lookup(super_handle, prototype_name)? {
                        Some(lookup) => match lookup.value() {
                            crate::object::PropertyValue::Data { value, .. } => value,
                            crate::object::PropertyValue::Accessor { .. } => {
                                RegisterValue::undefined()
                            }
                        },
                        None => RegisterValue::undefined(),
                    };
                    let super_proto_handle = if super_proto.is_null() {
                        None
                    } else if let Some(h) = super_proto
                        .as_object_handle()
                        .map(crate::object::ObjectHandle)
                    {
                        Some(h)
                    } else {
                        let err = runtime.alloc_type_error(
                            "Class extends value does not have a valid prototype",
                        )?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    };
                    runtime
                        .objects
                        .set_prototype(class_prototype_handle, super_proto_handle)
                        .map_err(|err| {
                            InterpreterError::NativeCall(Box::from(format!(
                                "SetClassHeritage: instance chain: {err:?}"
                            )))
                        })?;
                }
            }

            // §15.7.11 PropertyDefinitionEvaluation (class body,
            // method kind) — install `acc` as a non-enumerable
            // method data property. Spec-correct alternative to
            // the plain `StaNamedProperty` path, which would
            // otherwise leak class methods into `for…in` /
            // `Object.keys`. Computed-key variant reads the key
            // register; plain key uses the function's property
            // name table.
            Opcode::DefineClassMethod => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassMethod: target is not an object",
                    )));
                };
                let property = resolve_property(function, runtime, prop_id)?;
                let value = activation.accumulator();
                runtime
                    .objects
                    .define_own_property(
                        target_handle,
                        property,
                        crate::object::PropertyValue::data_with_attrs(
                            value,
                            crate::object::PropertyAttributes::builtin_method(),
                        ),
                    )
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "DefineClassMethod: define_own_property failed: {err:?}"
                        )))
                    })?;
            }
            Opcode::DefineClassMethodComputed => {
                let target_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassMethodComputed: target is not an object",
                    )));
                };
                let property = key_to_property_name(runtime, key)?;
                let value = activation.accumulator();
                runtime
                    .objects
                    .define_own_property(
                        target_handle,
                        property,
                        crate::object::PropertyValue::data_with_attrs(
                            value,
                            crate::object::PropertyAttributes::builtin_method(),
                        ),
                    )
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "DefineClassMethodComputed: define_own_property failed: {err:?}"
                        )))
                    })?;
            }

            // M29: Class field / accessor / private-name opcodes
            //
            // §6.2.12 / §15.7.14 infrastructure — the compiler
            // emits `AllocClassId` once per class-definition
            // block, which writes a fresh class id into the
            // frame's `current_class_id` scratch slot. Subsequent
            // `CopyClassId r_target` reads the slot and stamps
            // it onto `r_target` (a closure). All private-name
            // opcodes then pull the class_id from the active
            // closure at runtime.
            Opcode::AllocClassId => {
                activation.current_class_id = runtime.alloc_class_id();
            }
            Opcode::CopyClassId => {
                let target_reg = reg(&instr.operands, 0)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "CopyClassId: target is not an object",
                    )));
                };
                runtime
                    .objects
                    .set_closure_class_id(target_handle, activation.current_class_id)
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "CopyClassId: set_closure_class_id failed: {err:?}"
                        )))
                    })?;
            }

            // §15.7.14 step 28 — DefineField. Installs the
            // accumulator value as an enumerable, writable,
            // configurable data property on `r_target` under the
            // name interned at `name_idx`. Used by class field
            // initializer closures for `class C { x = expr; }`.
            Opcode::DefineField => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineField: target is not an object",
                    )));
                };
                let property = resolve_property(function, runtime, prop_id)?;
                let value = activation.accumulator();
                runtime
                    .objects
                    .define_own_property(
                        target_handle,
                        property,
                        crate::object::PropertyValue::data_with_attrs(
                            value,
                            crate::object::PropertyAttributes::data(),
                        ),
                    )
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "DefineField: define_own_property failed: {err:?}"
                        )))
                    })?;
            }
            // §15.7.14 — DefineComputedField. Like `DefineField`
            // but the key comes from a runtime register (the
            // evaluated computed-key expression, already coerced
            // via `ToPropertyKey` by the compiler).
            Opcode::DefineComputedField => {
                let target_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineComputedField: target is not an object",
                    )));
                };
                let property = key_to_property_name(runtime, key)?;
                let value = activation.accumulator();
                runtime
                    .objects
                    .define_own_property(
                        target_handle,
                        property,
                        crate::object::PropertyValue::data_with_attrs(
                            value,
                            crate::object::PropertyAttributes::data(),
                        ),
                    )
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "DefineComputedField: define_own_property failed: {err:?}"
                        )))
                    })?;
            }

            // §15.7.14 step 34 — install the compiled field
            // initializer closure onto the class constructor's
            // `[[Fields]]` slot (we collapse the array into a
            // single closure that walks all fields in order).
            // Called once per class body right after the
            // initializer is synthesized; later `RunClassFieldInitializer`
            // invokes it per instance.
            Opcode::SetClassFieldInitializer => {
                let class_reg = reg(&instr.operands, 0)?;
                let class_val = read_reg(activation, function, class_reg)?;
                let Some(class_handle) = class_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "SetClassFieldInitializer: target is not a closure",
                    )));
                };
                let initializer = activation.accumulator();
                let Some(init_handle) = initializer
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "SetClassFieldInitializer: initializer is not a closure",
                    )));
                };
                runtime
                    .objects
                    .set_closure_field_initializer(class_handle, init_handle)
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "SetClassFieldInitializer: {err:?}"
                        )))
                    })?;
            }

            // §15.7.14 / §10.2.1.3 — invoke the active function's
            // class field initializer with `r_this` as receiver.
            // Base ctors emit this at body entry; derived ctors
            // emit it after every `CallSuper` so field init runs
            // once the derived `this` binding is live. The
            // initializer's return value is discarded.
            Opcode::RunClassFieldInitializer => {
                let this_reg = reg(&instr.operands, 0)?;
                let this_val = read_reg(activation, function, this_reg)?;
                let Some(active_closure) = activation.closure_handle() else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "RunClassFieldInitializer: no active closure",
                    )));
                };
                let initializer = runtime
                    .objects
                    .closure_field_initializer(active_closure)
                    .map_err(|err| {
                        InterpreterError::NativeCall(Box::from(format!(
                            "RunClassFieldInitializer: lookup failed: {err:?}"
                        )))
                    })?;
                if let Some(init_handle) = initializer {
                    match self.call_callable_bytecode(runtime, init_handle, this_val, &[]) {
                        Ok(_) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                        }
                        Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                        Err(other) => return Ok(other),
                    }
                }
            }

            // §7.3.31 PrivateFieldAdd — append a private field
            // to `r_target`'s `[[PrivateElements]]` using
            // `{ class_id: activeClosure.class_id, description:
            // name }` as the key. Value comes from the
            // accumulator. Throws if the field already exists
            // on the target (§7.3.31 step 3).
            Opcode::DefinePrivateField => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefinePrivateField: target is not an object",
                    )));
                };
                let key = active_private_name_key(activation, function, runtime, prop_id)?;
                let value = activation.accumulator();
                if let Err(err) = runtime.objects.private_field_add(target_handle, key, value) {
                    return throw_object_error(runtime, err);
                }
            }
            // §7.3.32 PrivateGet — read a private field / method
            // / accessor from `r_obj`. Fields return the stored
            // value; methods return the callable handle; accessor
            // gets invoke the getter with `r_obj` as receiver.
            Opcode::GetPrivateField => {
                let obj_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let obj_val = read_reg(activation, function, obj_reg)?;
                let Some(obj_handle) = obj_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime
                        .alloc_type_error("Cannot read private field on a non-object value")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let key = active_private_name_key(activation, function, runtime, prop_id)?;
                // Cloning is necessary because the borrow on
                // `runtime.objects` conflicts with the `&mut self`
                // we need below for the accessor invocation path.
                let element = runtime
                    .objects
                    .private_elements_ref(obj_handle, &key)
                    .cloned();
                match element {
                    Some(crate::object::PrivateElement::Field(v)) => {
                        activation.set_accumulator(v);
                    }
                    Some(crate::object::PrivateElement::Method(m)) => {
                        activation.set_accumulator(RegisterValue::from_object_handle(m.0));
                    }
                    Some(crate::object::PrivateElement::Accessor {
                        getter: Some(g), ..
                    }) => match self.call_callable_bytecode(runtime, g, obj_val, &[]) {
                        Ok(v) => {
                            activation.refresh_open_upvalues_from_cells(runtime)?;
                            activation.set_accumulator(v);
                        }
                        Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                        Err(other) => return Ok(other),
                    },
                    Some(crate::object::PrivateElement::Accessor { getter: None, .. }) => {
                        let err = runtime.alloc_type_error("private accessor has no getter")?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                    None => {
                        let err = runtime.alloc_type_error(
                            "cannot access private field or method: object does not have the private member",
                        )?;
                        return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                    }
                }
            }
            // §7.3.33 PrivateSet — write a private field value
            // (or forward through an accessor's setter). The
            // accumulator carries the value; if an accessor
            // setter is selected `private_set` returns its
            // handle, which we then invoke.
            Opcode::SetPrivateField => {
                let obj_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let obj_val = read_reg(activation, function, obj_reg)?;
                let Some(obj_handle) = obj_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime
                        .alloc_type_error("Cannot set private field on a non-object value")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let key = active_private_name_key(activation, function, runtime, prop_id)?;
                let value = activation.accumulator();
                match runtime.objects.private_set(obj_handle, &key, value) {
                    Ok(None) => {}
                    Ok(Some(setter)) => {
                        match self.call_callable_bytecode(runtime, setter, obj_val, &[value]) {
                            Ok(_) => {
                                activation.refresh_open_upvalues_from_cells(runtime)?;
                            }
                            Err(StepOutcome::Throw(v)) => return Ok(StepOutcome::Throw(v)),
                            Err(other) => return Ok(other),
                        }
                    }
                    Err(err) => return throw_object_error(runtime, err),
                }
                // Assignment expression result is the RHS value.
                activation.set_accumulator(value);
            }

            // §13.10.1 `#x in obj` — returns `true` iff `obj`
            // has an own private element with this active
            // closure's class_id + name. Throws TypeError when
            // `obj` is a primitive per the spec's step 5.
            Opcode::InPrivate => {
                let obj_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let obj_val = read_reg(activation, function, obj_reg)?;
                let Some(obj_handle) = obj_val.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    let err = runtime.alloc_type_error("Cannot use 'in' on a non-object value")?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                };
                let key = active_private_name_key(activation, function, runtime, prop_id)?;
                match runtime.objects.private_element_find(obj_handle, &key) {
                    Ok(found) => {
                        activation.set_accumulator(RegisterValue::from_bool(found));
                    }
                    Err(err) => return throw_object_error(runtime, err),
                }
            }

            // §10.4.1.4 / §15.7.11 — install an accessor on
            // `r_target` with class semantics (non-enumerable,
            // configurable). Getter half comes from the
            // accumulator; existing setter half is preserved
            // when the named slot already holds an accessor.
            Opcode::DefineClassGetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassGetter: target is not an object",
                    )));
                };
                let property = resolve_property(function, runtime, prop_id)?;
                let getter = activation.accumulator();
                install_class_accessor(
                    runtime,
                    target_handle,
                    property,
                    AccessorHalf::Get,
                    getter,
                )?;
            }
            Opcode::DefineClassSetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassSetter: target is not an object",
                    )));
                };
                let property = resolve_property(function, runtime, prop_id)?;
                let setter = activation.accumulator();
                install_class_accessor(
                    runtime,
                    target_handle,
                    property,
                    AccessorHalf::Set,
                    setter,
                )?;
            }
            // Computed-key variants — key lives in a register.
            Opcode::DefineClassGetterComputed => {
                let target_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassGetterComputed: target is not an object",
                    )));
                };
                let property = key_to_property_name(runtime, key)?;
                let getter = activation.accumulator();
                install_class_accessor(
                    runtime,
                    target_handle,
                    property,
                    AccessorHalf::Get,
                    getter,
                )?;
            }
            Opcode::DefineClassSetterComputed => {
                let target_reg = reg(&instr.operands, 0)?;
                let key_reg = reg(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let key = read_reg(activation, function, key_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefineClassSetterComputed: target is not an object",
                    )));
                };
                let property = key_to_property_name(runtime, key)?;
                let setter = activation.accumulator();
                install_class_accessor(
                    runtime,
                    target_handle,
                    property,
                    AccessorHalf::Set,
                    setter,
                )?;
            }

            // M29.5: §15.7.14 step 28 / §7.3.33 — store a private
            // method or accessor on the class constructor's
            // `[[PrivateMethods]]` slot. `construct_callable` /
            // `super_call_dispatch` later copy the list to each
            // fresh instance's `[[PrivateElements]]`.
            Opcode::PushPrivateMethod => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateMethod: target is not a closure",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let method = activation.accumulator();
                let Some(method_handle) =
                    method.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateMethod: method is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.push_private_method(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Method(method_handle),
                ) {
                    return throw_object_error(runtime, err);
                }
            }
            Opcode::PushPrivateGetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateGetter: target is not a closure",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let getter = activation.accumulator();
                let Some(getter_handle) =
                    getter.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateGetter: getter is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.push_private_method(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: Some(getter_handle),
                        setter: None,
                    },
                ) {
                    return throw_object_error(runtime, err);
                }
            }
            Opcode::PushPrivateSetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateSetter: target is not a closure",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let setter = activation.accumulator();
                let Some(setter_handle) =
                    setter.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "PushPrivateSetter: setter is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.push_private_method(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: None,
                        setter: Some(setter_handle),
                    },
                ) {
                    return throw_object_error(runtime, err);
                }
            }

            // M29.5: §7.3.33 `PrivateMethodOrAccessorAdd` —
            // directly install a private method/accessor onto
            // `r_target`'s own `[[PrivateElements]]`. Emitted by
            // the compiler for `static #m() {}` / `static get #p()`
            // so the element lives on the class constructor
            // itself (no instance-copy step).
            Opcode::DefinePrivateMethod => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefinePrivateMethod: target is not an object",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let method = activation.accumulator();
                let Some(method_handle) =
                    method.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "DefinePrivateMethod: method is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.private_method_or_accessor_add(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Method(method_handle),
                ) {
                    return throw_object_error(runtime, err);
                }
            }
            Opcode::DefinePrivateGetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefinePrivateGetter: target is not an object",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let getter = activation.accumulator();
                let Some(getter_handle) =
                    getter.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "DefinePrivateGetter: getter is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.private_method_or_accessor_add(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: Some(getter_handle),
                        setter: None,
                    },
                ) {
                    return throw_object_error(runtime, err);
                }
            }
            Opcode::DefinePrivateSetter => {
                let target_reg = reg(&instr.operands, 0)?;
                let prop_id = idx_operand(&instr.operands, 1)?;
                let target_val = read_reg(activation, function, target_reg)?;
                let Some(target_handle) = target_val
                    .as_object_handle()
                    .map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::TypeError(Box::from(
                        "DefinePrivateSetter: target is not an object",
                    )));
                };
                let key = class_def_private_key(activation, function, prop_id)?;
                let setter = activation.accumulator();
                let Some(setter_handle) =
                    setter.as_object_handle().map(crate::object::ObjectHandle)
                else {
                    return Err(InterpreterError::NativeCall(Box::from(
                        "DefinePrivateSetter: setter is not a closure",
                    )));
                };
                if let Err(err) = runtime.objects.private_method_or_accessor_add(
                    target_handle,
                    key,
                    crate::object::PrivateElement::Accessor {
                        getter: None,
                        setter: Some(setter_handle),
                    },
                ) {
                    return throw_object_error(runtime, err);
                }
            }

            // §14.6.3 / §27.7.5.3 Await — drive microtasks +
            // expired timers until the operand settles, then
            // unwrap:
            //   - Fulfilled(v)  → acc = v.
            //   - Rejected(r)   → throw r.
            //   - Pending after budget → TypeError.
            //   - Non-promise   → acc unchanged per §27.7.5.3
            //     step 5 ("already fulfilled with itself").
            Opcode::Await => {
                let input = activation.accumulator();
                let Some(raw) = input.as_object_handle() else {
                    activation.set_pc(next_pc);
                    return Ok(StepOutcome::Continue);
                };
                let promise_handle = crate::object::ObjectHandle(raw);
                if runtime.objects.get_promise(promise_handle).is_none() {
                    // Non-promise object — pass through.
                    activation.set_pc(next_pc);
                    return Ok(StepOutcome::Continue);
                }
                self.drive_event_loop_until_settled(runtime, _module, promise_handle)?;
                let promise = runtime
                    .objects
                    .get_promise(promise_handle)
                    .expect("promise kind checked above");
                if let Some(value) = promise.fulfilled_value() {
                    activation.set_accumulator(value);
                } else if let Some(reason) = promise.rejected_reason() {
                    return Ok(StepOutcome::Throw(reason));
                } else {
                    let err = runtime.alloc_type_error(
                        "await on a pending promise exceeded the event-loop budget",
                    )?;
                    return Ok(StepOutcome::Throw(RegisterValue::from_object_handle(err.0)));
                }
            }

            // §14.4.14 YieldExpression — suspend the generator
            // at this point. The yielded value is in acc; the
            // outer `resume_generator_impl` step loop catches
            // `StepOutcome::GeneratorYield` to snapshot state
            // + return control to `.next()`. On the next
            // resume, acc is set to the caller-provided value
            // (or the accumulator slot is repurposed as the
            // `sent_value` drop-in).
            Opcode::Yield => {
                let yielded = activation.accumulator();
                // Advance PC first so resume continues AFTER
                // this Yield opcode rather than re-yielding in
                // an infinite loop.
                activation.set_pc(next_pc);
                return Ok(StepOutcome::GeneratorYield {
                    yielded_value: yielded,
                    // v2 uses the accumulator for the sent
                    // value on resume; the register slot is
                    // unused but kept for spec-alignment.
                    resume_register: 0,
                });
            }
            Opcode::SuspendGenerator => {
                // Explicit suspend-without-value form — used by
                // `gen.next()` start-path wrappers in some
                // codegen strategies. We emit `Yield` with
                // `undefined` instead in the source compiler,
                // so this path is rarely exercised; keep it
                // for opcode completeness.
                activation.set_pc(next_pc);
                return Ok(StepOutcome::GeneratorYield {
                    yielded_value: RegisterValue::undefined(),
                    resume_register: 0,
                });
            }

            Opcode::Nop => {}

            // Any other opcode is unsupported by this Phase 3b.1
            // skeleton. Phase 3b.6 fills in property access, calls,
            // generators, etc.
            other => {
                return Err(InterpreterError::NativeCall(Box::from(format!(
                    "v2 opcode {other:?} not yet implemented by dispatch"
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
    fn call_callable_bytecode(
        &self,
        runtime: &mut RuntimeState,
        callable: crate::object::ObjectHandle,
        receiver: RegisterValue,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, StepOutcome> {
        match runtime.call_callable(callable, receiver, arguments) {
            Ok(value) => Ok(value),
            Err(crate::VmNativeCallError::Thrown(value)) => Err(StepOutcome::Throw(value)),
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
    /// Unlike `call_callable_bytecode`, this does not need an ObjectHandle —
    /// the callee is statically known and has no captured environment
    /// (direct calls target top-level / non-capturing functions).
    fn call_direct_bytecode(
        &self,
        runtime: &mut RuntimeState,
        module: &Module,
        callee_idx: crate::module::FunctionIndex,
        arguments: &[RegisterValue],
    ) -> Result<RegisterValue, StepOutcome> {
        use crate::frame::{FrameFlags, FrameMetadata};

        let callee = match module.function(callee_idx) {
            Some(f) => f,
            None => match runtime.alloc_type_error("CallDirect: invalid function index") {
                Ok(h) => {
                    return Err(StepOutcome::Throw(RegisterValue::from_object_handle(h.0)));
                }
                Err(_) => return Err(StepOutcome::Throw(RegisterValue::undefined())),
            },
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

        // M33: §27.7.5.1 AsyncFunctionStart — direct calls to
        // `async function` declarations also need to route
        // through the promise-wrapping driver. Without this
        // branch, calling `inner()` where `inner` is an async
        // top-level function would return a plain value instead
        // of a Promise, so downstream `.then(...)` would fail.
        if callee.is_async() && !callee.is_generator() {
            return match Self::execute_async_function_body(runtime, module, &mut activation) {
                Ok(v) => Ok(v),
                Err(InterpreterError::UncaughtThrow(v)) => Err(StepOutcome::Throw(v)),
                Err(InterpreterError::TypeError(msg)) => match runtime.alloc_type_error(&msg) {
                    Ok(h) => Err(StepOutcome::Throw(RegisterValue::from_object_handle(h.0))),
                    Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
                },
                Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
            };
        }

        // M34: §27.5.1.1 / §27.6.1.1 — direct calls to
        // `function* ()` / `async function* ()` top-level
        // declarations allocate the matching generator object
        // instead of running the body. `.next()` on the
        // generator later drives `resume_generator_impl`.
        if callee.is_generator() {
            let gen_handle = if callee.is_async() {
                runtime.alloc_async_generator(module.clone(), callee_idx, None, arguments.to_vec())
            } else {
                runtime.alloc_generator(module.clone(), callee_idx, None, arguments.to_vec())
            };
            return Ok(RegisterValue::from_object_handle(gen_handle.0));
        }

        match self.run_with_tier_up(module, &mut activation, runtime) {
            Ok(super::Completion::Return(v)) => Ok(v),
            Ok(super::Completion::Throw(v)) => Err(StepOutcome::Throw(v)),
            Err(InterpreterError::UncaughtThrow(v)) => Err(StepOutcome::Throw(v)),
            Err(InterpreterError::TypeError(msg)) => match runtime.alloc_type_error(&msg) {
                Ok(h) => Err(StepOutcome::Throw(RegisterValue::from_object_handle(h.0))),
                Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
            },
            Err(_) => Err(StepOutcome::Throw(RegisterValue::undefined())),
        }
    }
}

// -------- operand / helper plumbing --------

fn reg(ops: &[Operand], pos: usize) -> Result<RegisterIndex, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Reg(r)) => {
            RegisterIndex::try_from(*r).map_err(|_| InterpreterError::RegisterOutOfBounds)
        }
        _ => Err(InterpreterError::NativeCall(Box::from(
            "v2 operand kind mismatch: expected Reg",
        ))),
    }
}

fn imm(ops: &[Operand], pos: usize) -> Result<i32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Imm(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(Box::from(
            "v2 operand kind mismatch: expected Imm",
        ))),
    }
}

fn idx_operand(ops: &[Operand], pos: usize) -> Result<u32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::Idx(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(Box::from(
            "v2 operand kind mismatch: expected Idx",
        ))),
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
        let r = RegisterIndex::try_from(
            base.checked_add(i)
                .ok_or(InterpreterError::RegisterOutOfBounds)?,
        )
        .map_err(|_| InterpreterError::RegisterOutOfBounds)?;
        out.push(activation.read_bytecode_register(function, r)?);
    }
    Ok(out)
}

/// Outcome of [`super_call_dispatch`]. `Continue` means the
/// derived-ctor frame is still running and the interpreter should
/// advance PC normally; `Throw` lifts a runtime error into the
/// caller so it can convert into a `StepOutcome::Throw`.
enum SuperCallOutcome {
    Continue,
    Throw(RegisterValue),
}

/// §13.3.7.1 SuperCall — shared implementation used by both the
/// fixed-arity `CallSuper` and spread-arity `CallSuperSpread`
/// opcodes. Resolves the super constructor from the active
/// closure's `[[GetPrototypeOf]]()`, forwards `[[NewTarget]]`,
/// invokes `construct_callable`, and writes the resulting receiver
/// into the derived frame's `this` slot (and the accumulator) per
/// §10.2.1.3 step 12.
fn super_call_dispatch(
    activation: &mut Activation,
    function: &Function,
    runtime: &mut RuntimeState,
    args: &[RegisterValue],
) -> Result<SuperCallOutcome, InterpreterError> {
    let Some(active_closure) = activation.closure_handle() else {
        return Err(InterpreterError::NativeCall(Box::from(
            "super call: active closure is missing",
        )));
    };
    let super_ctor = match runtime
        .objects
        .get_prototype(active_closure)
        .map_err(|err| {
            InterpreterError::NativeCall(Box::from(format!(
                "super call: prototype lookup failed: {err:?}"
            )))
        })? {
        Some(handle) => handle,
        None => {
            let err = runtime.alloc_type_error("Super constructor is null")?;
            return Ok(SuperCallOutcome::Throw(RegisterValue::from_object_handle(
                err.0,
            )));
        }
    };
    if !runtime.is_constructible(super_ctor) {
        let err = runtime.alloc_type_error("Super constructor is not a constructor")?;
        return Ok(SuperCallOutcome::Throw(RegisterValue::from_object_handle(
            err.0,
        )));
    }
    // §10.2.1.3 step 6: `[[NewTarget]]` must be present. Walk arrow
    // closures up until we find a frame or closure that carries it.
    let new_target = activation.construct_new_target().or_else(|| {
        runtime
            .objects
            .closure_captured_new_target(active_closure)
            .ok()
            .flatten()
    });
    let Some(new_target) = new_target else {
        let err = runtime
            .alloc_reference_error("super call is only valid inside a derived constructor")?;
        return Ok(SuperCallOutcome::Throw(RegisterValue::from_object_handle(
            err.0,
        )));
    };
    // §13.3.7.1 step 9 — `this` must not yet be initialized.
    if let Some(slot) = function.frame_layout().receiver_slot() {
        let current = activation.register(slot)?;
        if current.as_object_handle().is_some() {
            let err = runtime.alloc_reference_error("Super constructor may only be called once")?;
            return Ok(SuperCallOutcome::Throw(RegisterValue::from_object_handle(
                err.0,
            )));
        }
    }
    let result = runtime.construct_callable(super_ctor, args, new_target);
    let receiver = match result {
        Ok(value) => value,
        Err(crate::VmNativeCallError::Thrown(value)) => {
            return Ok(SuperCallOutcome::Throw(value));
        }
        Err(crate::VmNativeCallError::Internal(msg)) => {
            return Err(InterpreterError::NativeCall(msg));
        }
    };
    // §10.2.1.3 step 12 — initialize the `this` binding with the
    // freshly constructed receiver.
    if let Some(slot) = function.frame_layout().receiver_slot() {
        activation.set_register(slot, receiver)?;
    }
    activation.set_accumulator(receiver);

    // §7.3.33 InitializeInstanceElements — copy the derived
    // class's own private methods + accessors onto the instance.
    // Parent's private methods were already installed when
    // parent's constructor ran (either nested via super-chain
    // `construct_callable` or at the top of this same helper
    // for the parent's frame).
    if let Some(receiver_handle) = receiver.as_object_handle().map(crate::object::ObjectHandle) {
        match runtime.install_class_private_methods(active_closure, receiver_handle) {
            Ok(()) => {}
            Err(crate::VmNativeCallError::Thrown(v)) => {
                return Ok(SuperCallOutcome::Throw(v));
            }
            Err(crate::VmNativeCallError::Internal(msg)) => {
                return Err(InterpreterError::NativeCall(msg));
            }
        }
    }

    // §15.7.14 step 28 — run the derived class's own field
    // initializer now that `this` is live. Mirrors the
    // base-class path in `construct_callable`; the two together
    // guarantee field init happens exactly once per instance.
    if let Ok(Some(init)) = runtime.objects.closure_field_initializer(active_closure) {
        match runtime.call_callable(init, receiver, &[]) {
            Ok(_) => {}
            Err(crate::VmNativeCallError::Thrown(v)) => {
                return Ok(SuperCallOutcome::Throw(v));
            }
            Err(crate::VmNativeCallError::Internal(msg)) => {
                return Err(InterpreterError::NativeCall(msg));
            }
        }
    }
    Ok(SuperCallOutcome::Continue)
}

/// Which side of an accessor pair a `Define*Getter/Setter` opcode
/// is currently installing. Used by [`install_class_accessor`] to
/// preserve the other half of an existing accessor when a class
/// declaration binds a getter+setter under the same key.
#[derive(Clone, Copy)]
enum AccessorHalf {
    Get,
    Set,
}

/// §15.7.11 PropertyDefinitionEvaluation (class body) — installs
/// the accumulator-carried closure as one half of an accessor pair
/// on `target`. Class accessors are non-enumerable + configurable
/// (vs. enumerable for object-literal accessors), which is why
/// this is a dedicated helper rather than sharing the
/// object-literal path.
fn install_class_accessor(
    runtime: &mut RuntimeState,
    target: crate::object::ObjectHandle,
    property: crate::property::PropertyNameId,
    half: AccessorHalf,
    closure_value: RegisterValue,
) -> Result<(), InterpreterError> {
    let Some(closure_handle) = closure_value
        .as_object_handle()
        .map(crate::object::ObjectHandle)
    else {
        return Err(InterpreterError::TypeError(Box::from(
            "class accessor install: value is not a closure",
        )));
    };
    let desc = match half {
        AccessorHalf::Get => crate::object::PropertyDescriptor::accessor(
            Some(Some(closure_handle)),
            None,
            Some(false),
            Some(true),
        ),
        AccessorHalf::Set => crate::object::PropertyDescriptor::accessor(
            None,
            Some(Some(closure_handle)),
            Some(false),
            Some(true),
        ),
    };
    match runtime
        .objects
        .define_own_property_from_descriptor(target, property, desc)
    {
        Ok(_) => Ok(()),
        Err(err) => Err(InterpreterError::NativeCall(Box::from(format!(
            "class accessor install: {err:?}"
        )))),
    }
}

/// M29.5: `PrivateNameKey` used during class-definition
/// emission. Reads the `class_id` from the frame's
/// `current_class_id` scratch slot (populated by `AllocClassId`)
/// rather than from the active closure — during class evaluation
/// the active closure is the enclosing function, which has a
/// different (usually zero) class_id. Private method/accessor
/// installers (`PushPrivate*` / `DefinePrivate*`) all go through
/// this path so they consistently use the class-being-defined's
/// id.
fn class_def_private_key(
    activation: &Activation,
    function: &Function,
    prop_id: u32,
) -> Result<crate::object::PrivateNameKey, InterpreterError> {
    let class_id = activation.current_class_id;
    if class_id == 0 {
        return Err(InterpreterError::NativeCall(Box::from(
            "private element install: class_id not allocated",
        )));
    }
    let id = crate::property::PropertyNameId(prop_id as u16);
    let description = function
        .property_names()
        .get(id)
        .ok_or(InterpreterError::UnknownPropertyName)?
        .to_owned()
        .into_boxed_str();
    Ok(crate::object::PrivateNameKey {
        class_id,
        description,
    })
}

/// Construct a `PrivateNameKey` from the active closure's
/// `class_id` plus the name interned at `prop_id`. Used by every
/// private-field opcode to resolve to the right `[[PrivateElements]]`
/// bucket per §6.2.12. A zero `class_id` means the compiler failed
/// to emit `AllocClassId` / `CopyClassId` — surfaced as an internal
/// error rather than silently producing lookup misses.
fn active_private_name_key(
    activation: &Activation,
    function: &Function,
    runtime: &mut RuntimeState,
    prop_id: u32,
) -> Result<crate::object::PrivateNameKey, InterpreterError> {
    let Some(active_closure) = activation.closure_handle() else {
        return Err(InterpreterError::NativeCall(Box::from(
            "private name access: no active closure",
        )));
    };
    let class_id = runtime
        .objects
        .closure_class_id(active_closure)
        .map_err(|err| {
            InterpreterError::NativeCall(Box::from(format!(
                "private name access: class_id lookup failed: {err:?}"
            )))
        })?;
    if class_id == 0 {
        return Err(InterpreterError::NativeCall(Box::from(
            "private name access: active closure has no class_id",
        )));
    }
    let id = crate::property::PropertyNameId(prop_id as u16);
    let description = function
        .property_names()
        .get(id)
        .ok_or(InterpreterError::UnknownPropertyName)?
        .to_owned()
        .into_boxed_str();
    Ok(crate::object::PrivateNameKey {
        class_id,
        description,
    })
}

/// Convert an `ObjectError::TypeError` into a JS-level
/// `StepOutcome::Throw` carrying a fresh `TypeError` instance.
/// Other object errors fall back to an internal `NativeCall`
/// error so bugs surface loudly.
fn throw_object_error(
    runtime: &mut RuntimeState,
    err: crate::object::ObjectError,
) -> Result<StepOutcome, InterpreterError> {
    match err {
        crate::object::ObjectError::TypeError(msg) => {
            let handle = runtime.alloc_type_error(&msg)?;
            Ok(StepOutcome::Throw(RegisterValue::from_object_handle(
                handle.0,
            )))
        }
        other => Err(InterpreterError::NativeCall(Box::from(format!(
            "{other:?}"
        )))),
    }
}

/// §13.3.7 SuperReference base lookup — resolves
/// `activeFunction.[[HomeObject]].[[GetPrototypeOf]]()` per
/// `MakeSuperPropertyReference`. Surfaced as a TypeError when the
/// active function has no `[[HomeObject]]` (e.g. a regular
/// function-level closure) or when the home object's prototype is
/// null — both paths match the spec's "must throw" cases for
/// super-property access outside a method / on a rootless class.
fn super_property_base(
    activation: &Activation,
    runtime: &mut RuntimeState,
) -> Result<crate::object::ObjectHandle, InterpreterError> {
    let Some(active) = activation.closure_handle() else {
        return Err(InterpreterError::NativeCall(Box::from(
            "super property reference: active closure is missing",
        )));
    };
    let home = runtime
        .objects
        .closure_home_object(active)
        .map_err(|err| {
            InterpreterError::NativeCall(Box::from(format!(
                "super property reference: home lookup failed: {err:?}"
            )))
        })?
        .ok_or_else(|| {
            InterpreterError::TypeError(Box::from(
                "super property reference requires an enclosing method",
            ))
        })?;
    let proto = runtime
        .objects
        .get_prototype(home)
        .map_err(|err| {
            InterpreterError::NativeCall(Box::from(format!(
                "super property reference: proto lookup failed: {err:?}"
            )))
        })?
        .ok_or_else(|| {
            InterpreterError::TypeError(Box::from(
                "super property reference on rootless prototype chain",
            ))
        })?;
    Ok(proto)
}

/// Resolve a `PropertyNameId` into a runtime-interned id via the
/// function's property-name side table. Mirrors
/// `Interpreter::resolve_property_name` from v1 dispatch but takes a
/// raw u32 (the v2 `Idx` operand) instead of a v1 `RegisterIndex`.
/// P1: Returns the active [`PropertyFeedback`] for a
/// `LdaNamedProperty`/`StaNamedProperty` PC if one was allocated
/// at compile time and the feedback vector layout has grown to
/// include it. `None` when the op has no attached slot (older
/// emission sites that haven't been migrated yet) or when the
/// runtime layout doesn't declare the slot as `Property` kind.
fn property_feedback_for_pc<'a>(
    function: &Function,
    feedback_vector: &'a crate::feedback::FeedbackVector,
    pc: u32,
) -> Option<&'a crate::feedback::PropertyFeedback> {
    let bytecode_slot = function.bytecode().feedback().get(pc)?;
    let slot = crate::feedback::FeedbackSlotId(bytecode_slot.0);
    let layout = function.feedback().get(slot)?;
    if layout.kind() != crate::feedback::FeedbackKind::Property {
        return None;
    }
    feedback_vector.property(slot)
}

/// P1: Probes the polymorphic inline cache against the current
/// object's shape. Walks at most 4 cached `(shape_id, slot_index)`
/// pairs; on a match returns the cached value via `get_shaped`,
/// otherwise returns `Ok(None)` so the caller falls through to
/// `ordinary_get`. Megamorphic / uninitialised feedback pins the
/// slow path.
fn probe_property_inline_cache(
    feedback: &crate::feedback::PropertyFeedback,
    objects: &crate::object::ObjectHeap,
    handle: crate::object::ObjectHandle,
) -> Result<Option<RegisterValue>, InterpreterError> {
    use crate::feedback::PropertyFeedback;
    let caches: &[crate::object::PropertyInlineCache] = match feedback {
        PropertyFeedback::Monomorphic(cache) => std::slice::from_ref(cache),
        PropertyFeedback::Polymorphic(caches) => caches.as_slice(),
        PropertyFeedback::Uninitialized | PropertyFeedback::Megamorphic => return Ok(None),
    };
    for cache in caches {
        match objects.get_shaped(handle, cache.shape_id(), cache.slot_index()) {
            Ok(Some(prop)) => {
                // §9.1.9.1 OrdinaryGet — only the data-property
                // fast path applies at the IC; accessors bail to
                // the slow path because calling the getter needs
                // the full receiver chain.
                if let crate::object::PropertyValue::Data { value, .. } = prop {
                    return Ok(Some(value));
                }
                return Ok(None);
            }
            Ok(None) => continue,
            // Invalid heap kinds fall through to the generic path —
            // a real error there still surfaces via `ordinary_get`.
            Err(_) => continue,
        }
    }
    Ok(None)
}

fn resolve_property(
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

fn add_disposable_resource(
    runtime: &mut RuntimeState,
    activation: &mut Activation,
    value: RegisterValue,
    await_dispose: bool,
) -> Result<(), RegisterValue> {
    let Some(entry) = prepare_using_entry(runtime, value, await_dispose)? else {
        return Ok(());
    };
    activation.push_using_entry(entry);
    Ok(())
}

fn prepare_using_entry(
    runtime: &mut RuntimeState,
    value: RegisterValue,
    await_dispose: bool,
) -> Result<Option<UsingEntry>, RegisterValue> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(None);
    }

    let target = runtime.property_base_object_handle(value).map_err(|err| {
        let message = err.to_string();
        type_error_value(runtime, &message).unwrap_or_else(|_| RegisterValue::undefined())
    })?;

    let async_prop = runtime
        .intern_symbol_property_name(crate::intrinsics::WellKnownSymbol::AsyncDispose.stable_id());
    let sync_prop = runtime
        .intern_symbol_property_name(crate::intrinsics::WellKnownSymbol::Dispose.stable_id());

    let (method, effective_await) = if await_dispose {
        let async_method = get_method_for_using(runtime, target, async_prop, value)?;
        if async_method != RegisterValue::undefined() {
            (async_method, true)
        } else {
            (
                get_method_for_using(runtime, target, sync_prop, value)?,
                true,
            )
        }
    } else {
        (
            get_method_for_using(runtime, target, sync_prop, value)?,
            false,
        )
    };

    let Some(disposer) = method.as_object_handle().map(crate::object::ObjectHandle) else {
        if method == RegisterValue::undefined() {
            let message = if await_dispose {
                "Object is not async disposable"
            } else {
                "Object is not disposable"
            };
            return Err(type_error_value(runtime, message)?);
        }
        return Err(type_error_value(
            runtime,
            "Dispose method must be callable",
        )?);
    };
    if !runtime.objects.is_callable(disposer) {
        return Err(type_error_value(
            runtime,
            "Dispose method must be callable",
        )?);
    }

    Ok(Some(UsingEntry::new(value, disposer, effective_await)))
}

fn get_method_for_using(
    runtime: &mut RuntimeState,
    target: crate::object::ObjectHandle,
    property: crate::property::PropertyNameId,
    receiver: RegisterValue,
) -> Result<RegisterValue, RegisterValue> {
    if runtime.is_proxy(target) {
        runtime
            .proxy_get(target, property, receiver)
            .map_err(|err| {
                type_error_value(runtime, err.to_string().as_str())
                    .unwrap_or_else(|_| RegisterValue::undefined())
            })
    } else {
        runtime
            .ordinary_get(target, property, receiver)
            .map_err(|err| match err {
                crate::VmNativeCallError::Thrown(value) => value,
                crate::VmNativeCallError::Internal(message) => type_error_value(runtime, &message)
                    .unwrap_or_else(|_| RegisterValue::undefined()),
            })
    }
}

fn dispose_using_scope(
    interpreter: &Interpreter,
    runtime: &mut RuntimeState,
    module: &Module,
    activation: &mut Activation,
) -> Result<(), InterpreterError> {
    let Some(scope_start) = activation.pop_using_scope() else {
        return Err(InterpreterError::NativeCall(
            "DisposeUsingScope without matching PushUsingScope".into(),
        ));
    };

    let had_pending_exception = activation.pending_exception().is_some();
    let mut current_throw =
        activation
            .pending_exception()
            .or_else(|| match activation.pending_abrupt_completion() {
                Some(PendingAbruptCompletion::Throw(value)) => Some(value),
                _ => None,
            });

    while activation.using_entry_count() > scope_start {
        let entry = activation
            .pop_using_entry()
            .expect("using entry count checked before pop");
        if let Err(dispose_error) = run_using_disposer(interpreter, runtime, module, entry) {
            current_throw = Some(match current_throw {
                Some(previous) => crate::intrinsics::error_class::alloc_suppressed_error_value(
                    runtime,
                    dispose_error,
                    previous,
                    RegisterValue::undefined(),
                )
                .map_err(|err| match err {
                    crate::VmNativeCallError::Thrown(value) => {
                        InterpreterError::UncaughtThrow(value)
                    }
                    crate::VmNativeCallError::Internal(message) => {
                        InterpreterError::NativeCall(message)
                    }
                })?,
                None => dispose_error,
            });
        }
    }

    if had_pending_exception {
        if let Some(value) = current_throw {
            activation.set_pending_exception(value);
        }
    } else if let Some(value) = current_throw {
        activation.set_pending_abrupt_completion(PendingAbruptCompletion::Throw(value));
    }

    Ok(())
}

fn run_using_disposer(
    interpreter: &Interpreter,
    runtime: &mut RuntimeState,
    module: &Module,
    entry: UsingEntry,
) -> Result<(), RegisterValue> {
    let result = runtime.call_callable(entry.disposer(), entry.receiver(), &[]);
    let value = match result {
        Ok(value) => value,
        Err(crate::VmNativeCallError::Thrown(value)) => return Err(value),
        Err(crate::VmNativeCallError::Internal(message)) => {
            return Err(type_error_value(runtime, &message)?);
        }
    };

    if !entry.await_dispose() {
        return Ok(());
    }

    let Some(raw) = value.as_object_handle() else {
        return Ok(());
    };
    let promise_handle = crate::object::ObjectHandle(raw);
    if runtime.objects.get_promise(promise_handle).is_none() {
        return Ok(());
    }

    interpreter
        .drive_event_loop_until_settled(runtime, module, promise_handle)
        .map_err(|err| {
            type_error_value(runtime, err.to_string().as_str())
                .unwrap_or_else(|_| RegisterValue::undefined())
        })?;
    let promise = runtime
        .objects
        .get_promise(promise_handle)
        .expect("promise kind checked above");
    if promise.fulfilled_value().is_some() {
        return Ok(());
    }
    if let Some(reason) = promise.rejected_reason() {
        return Err(reason);
    }
    Err(type_error_value(
        runtime,
        "await using on a pending promise exceeded the event-loop budget",
    )?)
}

fn type_error_value(
    runtime: &mut RuntimeState,
    message: &str,
) -> Result<RegisterValue, RegisterValue> {
    runtime
        .alloc_type_error(message)
        .map(|handle| RegisterValue::from_object_handle(handle.0))
        .map_err(|_| RegisterValue::undefined())
}

/// Coerce a `RegisterValue` to a property-name id per §7.1.19
/// ToPropertyKey. Three cases: symbols intern into their own
/// namespace via `intern_symbol_property_name` (so
/// `obj[Symbol.iterator]` lives in a separate bucket from
/// `obj["Symbol.iterator"]`), string objects pull their text out,
/// everything else stringifies via `ToString` and interns.
fn key_to_property_name(
    runtime: &mut RuntimeState,
    key: RegisterValue,
) -> Result<crate::property::PropertyNameId, InterpreterError> {
    // Symbol path: §7.1.19 step 2 keeps symbols as-is.
    if let Some(symbol_id) = key.as_symbol_id() {
        return Ok(runtime.intern_symbol_property_name(symbol_id));
    }
    // String fast path: key is already a string object — pull
    // its text out.
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
        _ => Err(InterpreterError::NativeCall(Box::from(
            "v2 operand kind mismatch: expected JumpOff",
        ))),
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

/// §7.1.6 ToInt32 — fast int32 pass-through, slow path falls back to
/// the runtime helper (handles Booleans, Number objects, strings,
/// `null`/`undefined`, etc. per §7.1.4 ToNumber). Returns both coerced
/// operands together with the arithmetic feedback tag: `Int32` when
/// both were already int32, `Any` when any slow-path coercion ran.
fn coerce_int32_pair(
    runtime: &mut crate::interpreter::RuntimeState,
    lhs: RegisterValue,
    rhs: RegisterValue,
) -> Result<(i32, i32, ArithmeticFeedback), InterpreterError> {
    match (lhs.as_i32(), rhs.as_i32()) {
        (Some(l), Some(r)) => Ok((l, r, ArithmeticFeedback::Int32)),
        (Some(l), None) => Ok((l, runtime.js_to_int32(rhs)?, ArithmeticFeedback::Any)),
        (None, Some(r)) => Ok((runtime.js_to_int32(lhs)?, r, ArithmeticFeedback::Any)),
        (None, None) => Ok((
            runtime.js_to_int32(lhs)?,
            runtime.js_to_int32(rhs)?,
            ArithmeticFeedback::Any,
        )),
    }
}

fn jump_target(end_pc: u32, offset: i32) -> u32 {
    let t = i64::from(end_pc) + i64::from(offset);
    u32::try_from(t).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crate::bytecode::{BytecodeBuilder, Opcode, Operand};
    use crate::frame::FrameLayout;
    use crate::module::{Function, FunctionIndex, Module};
    use crate::value::RegisterValue;

    use super::super::{Interpreter, RuntimeState};

    /// Runs a v2-only function against the real `RuntimeState` +
    /// `Interpreter` pipeline: builds a Module with a single Function,
    /// attaches the v2 bytecode, calls `execute_with_runtime`, and
    /// returns the resulting value.
    fn run_bytecode(
        build_fn: impl FnOnce(&mut BytecodeBuilder),
        register_count: u16,
        initial_regs: &[RegisterValue],
    ) -> RegisterValue {
        let mut builder = BytecodeBuilder::new();
        build_fn(&mut builder);
        let v2 = builder.finish().expect("build v2 bytecode");
        let layout = FrameLayout::new(0, 0, register_count, 0).expect("layout");
        let function = Function::with_empty_tables(Some("test"), layout, v2);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("valid module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result = interpreter
            .execute_with_runtime(&module, FunctionIndex(0), initial_regs, &mut runtime)
            .expect("execute_with_runtime");
        result.return_value()
    }

    /// Like `run_bytecode`, but lets the caller preseed the property-name and
    /// string-literal side tables. Used by the tests that exercise
    /// named/keyed property access opcodes.
    fn run_bytecode_with_tables(
        build_fn: impl FnOnce(&mut BytecodeBuilder),
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
        build_fn(&mut builder);
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
        let function = Function::new(Some("test"), layout, v2, tables);
        let module =
            Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("valid module");
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
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::LdaSmi, &[Operand::Imm(42)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn add_two_regs_via_accumulator() {
        // r0 = 10, r1 = 32; Ldar r0; Add r1; Return (→ 42).
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(Opcode::Add, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            2,
            &[RegisterValue::from_i32(10), RegisterValue::from_i32(32)],
        );
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn smi_variants_compute_correctly() {
        // (((5 + 3) - 2) * 2) | 0 = 12
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::LdaSmi, &[Operand::Imm(5)]).unwrap();
                b.emit(Opcode::AddSmi, &[Operand::Imm(3)]).unwrap();
                b.emit(Opcode::SubSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(Opcode::MulSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(Opcode::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(12));
    }

    #[test]
    fn shifts_mask_to_low_five_bits() {
        // 0xDEAD_BEEF_u32 as i32 >>> 4 = 0x0DEA_DBEE
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::LdaSmi, &[Operand::Imm(0xDEAD_BEEFu32 as i32)])
                    .unwrap();
                b.emit(Opcode::ShrSmi, &[Operand::Imm(4)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
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
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::LdaTrue, &[]).unwrap();
                b.emit(Opcode::LogicalNot, &[]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_bool(), Some(false));
    }

    #[test]
    fn inc_dec_on_accumulator() {
        // 10; Inc; Inc; Dec; Return → 11
        let result = run_bytecode(
            |b| {
                b.emit(Opcode::LdaSmi, &[Operand::Imm(10)]).unwrap();
                b.emit(Opcode::Inc, &[]).unwrap();
                b.emit(Opcode::Inc, &[]).unwrap();
                b.emit(Opcode::Dec, &[]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(11));
    }

    #[test]
    fn null_undefined_jumps() {
        // if (acc === null) return 1 else return 2.
        let result = run_bytecode(
            |b| {
                let then_label = b.new_label();
                b.emit(Opcode::LdaNull, &[]).unwrap();
                b.emit_jump_to(Opcode::JumpIfNull, then_label).unwrap();
                b.emit(Opcode::LdaSmi, &[Operand::Imm(2)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
                b.bind_label(then_label).unwrap();
                b.emit(Opcode::LdaSmi, &[Operand::Imm(1)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            0,
            &[],
        );
        assert_eq!(result.as_i32(), Some(1));
    }

    #[test]
    fn div_and_mod() {
        // Exact division stays on the int32 fast path: 20 / 5 =
        // 4, 20 % 5 = 0. (20/5) + (20%5) = 4 — single int32
        // result. Real-valued division (17 / 5 = 3.4) falls
        // through to `js_divide`, so we pick an exact divisor
        // here to keep this test focused on the fast-path and
        // int-only Mod.
        let result = run_bytecode(
            |b| {
                // r0 = 20, r1 = 5
                b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(Opcode::Div, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
                b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
                b.emit(Opcode::Mod, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::Add, &[Operand::Reg(2)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
            },
            3,
            &[
                RegisterValue::from_i32(20),
                RegisterValue::from_i32(5),
                RegisterValue::undefined(),
            ],
        );
        assert_eq!(result.as_i32(), Some(4));
    }

    #[test]
    fn create_object_and_get_set_named_property() {
        // const o = {}; o.x = 7; return o.x + 5  →  12
        //
        // PropertyNameTable is pre-seeded with "x" at PropertyNameId(0).
        let (result, _runtime) = run_bytecode_with_tables(
            |b| {
                b.emit(Opcode::CreateObject, &[]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(0)]).unwrap();
                // o.x = 7
                b.emit(Opcode::LdaSmi, &[Operand::Imm(7)]).unwrap();
                b.emit(
                    Opcode::StaNamedProperty,
                    &[Operand::Reg(0), Operand::Idx(0)],
                )
                .unwrap();
                // acc = o.x
                b.emit(
                    Opcode::LdaNamedProperty,
                    &[Operand::Reg(0), Operand::Idx(0)],
                )
                .unwrap();
                b.emit(Opcode::AddSmi, &[Operand::Imm(5)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
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
        let (result, _runtime) = run_bytecode_with_tables(
            |b| {
                // r0 = {}
                b.emit(Opcode::CreateObject, &[]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(0)]).unwrap();
                // r1 = "y"
                b.emit(Opcode::LdaConstStr, &[Operand::Idx(0)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
                // acc = 100; o[r1] = acc
                b.emit(Opcode::LdaSmi, &[Operand::Imm(100)]).unwrap();
                b.emit(
                    Opcode::StaKeyedProperty,
                    &[Operand::Reg(0), Operand::Reg(1)],
                )
                .unwrap();
                // acc = r1; LdaKeyedProperty r0 → acc = r0[acc]
                b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::LdaKeyedProperty, &[Operand::Reg(0)])
                    .unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
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
        builder.emit(Opcode::LdaTheHole, &[]).unwrap();
        builder.emit(Opcode::AssertNotHole, &[]).unwrap();
        builder.emit(Opcode::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
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
        builder.emit(Opcode::LdaSmi, &[Operand::Imm(42)]).unwrap();
        builder.emit(Opcode::TypeOf, &[]).unwrap();
        builder.emit(Opcode::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
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
        let result = run_bytecode(
            |b| {
                // init s=0, i=0
                b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::LdaSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();

                let loop_header = b.new_label();
                let exit = b.new_label();
                b.bind_label(loop_header).unwrap();
                b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
                b.emit(Opcode::TestLessThan, &[Operand::Reg(0)]).unwrap();
                b.emit_jump_to(Opcode::JumpIfToBooleanFalse, exit).unwrap();

                // body: s = (s + i) | 0
                b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::Add, &[Operand::Reg(2)]).unwrap();
                b.emit(Opcode::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();

                // i = i + 1
                b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
                b.emit(Opcode::AddSmi, &[Operand::Imm(1)]).unwrap();
                b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();

                b.emit_jump_to(Opcode::Jump, loop_header).unwrap();

                b.bind_label(exit).unwrap();
                b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
                b.emit(Opcode::Return, &[]).unwrap();
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
        callee_b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::Add, &[Operand::Reg(1)]).unwrap();
        callee_b.emit(Opcode::Return, &[]).unwrap();
        let callee_bc = callee_b.finish().unwrap();
        // FrameLayout::new(hidden, params, locals, temps) — 2 params, no hidden slots.
        let callee_layout = FrameLayout::new(0, 2, 0, 0).unwrap();
        let callee = Function::with_empty_tables(Some("sum"), callee_layout, callee_bc);

        // Caller (fn_index 0): function() {
        //   r0 = 10; r1 = 32;
        //   acc = CallDirect(fn_index=1, args=[r0, r1]);
        //   return acc;
        // }
        // Register layout: r0/r1 are the arg slots (no params, 0 hidden, 2 locals).
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(10)]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(0)]).unwrap();
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(32)]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                Opcode::CallDirect,
                &[Operand::Idx(1), Operand::RegList { base: 0, count: 2 }],
            )
            .unwrap();
        caller_b.emit(Opcode::Return, &[]).unwrap();
        let caller_bc = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_empty_tables(Some("main"), caller_layout, caller_bc);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let interpreter = Interpreter::new();
        let result =
            match interpreter.execute_with_runtime(&module, FunctionIndex(0), &[], &mut runtime) {
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
        callee_b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::Add, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::Return, &[]).unwrap();
        let callee_bc = callee_b.finish().unwrap();
        let callee_layout = FrameLayout::new(0, 1, 0, 0).unwrap();
        let callee = Function::with_empty_tables(Some("double"), callee_layout, callee_bc);

        // Caller (fn_index 0):
        //   r0 = <closure>       (preseeded)
        //   r1 = 21
        //   acc = Call r0(undefined, [r1])
        //   return acc
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(21)]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                Opcode::CallUndefinedReceiver,
                &[Operand::Reg(0), Operand::RegList { base: 1, count: 1 }],
            )
            .unwrap();
        caller_b.emit(Opcode::Return, &[]).unwrap();
        let caller_bc = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_empty_tables(Some("main"), caller_layout, caller_bc);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");

        // Build the runtime, enter the module (so alloc_closure can find it),
        // and allocate a closure pointing at fn_index 1. Stuff the resulting
        // handle into the caller's r0 via `execute_with_runtime`'s preseed
        // argument list.
        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle =
            runtime.alloc_closure(FunctionIndex(1), Vec::new(), ObjClosureFlags::default());
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter.execute_with_runtime(
            &module,
            FunctionIndex(0),
            &preseed,
            &mut runtime,
        ) {
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
        b.emit(Opcode::ForInEnumerate, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::ForInNext, &[Operand::Reg(2), Operand::Reg(1)])
            .unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(3)]).unwrap();
        b.emit(Opcode::ForInNext, &[Operand::Reg(4), Operand::Reg(1)])
            .unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(5)]).unwrap();
        b.emit(Opcode::ForInNext, &[Operand::Reg(6), Operand::Reg(1)])
            .unwrap();
        // acc is now `true` (done on third step). Return it.
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 7, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

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
        b.emit(Opcode::LdaSmi, &[Operand::Imm(10)]).unwrap();
        b.emit(Opcode::ArrayPush, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(20)]).unwrap();
        b.emit(Opcode::ArrayPush, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::LdaSmi, &[Operand::Imm(30)]).unwrap();
        b.emit(Opcode::ArrayPush, &[Operand::Reg(0)]).unwrap();
        // Return the array itself (acc = Ldar r0).
        b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 1, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

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
            assert_eq!(elements[i].as_i32(), Some(*expected), "index {i} mismatch");
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
        b.emit(Opcode::GetIterator, &[Operand::Reg(0)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::IteratorNext, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::IteratorNext, &[Operand::Reg(1)]).unwrap();
        b.emit(Opcode::Star, &[Operand::Reg(3)]).unwrap();
        b.emit(Opcode::IteratorNext, &[Operand::Reg(1)]).unwrap();
        // acc is now undefined; ignore. Compute r2 + r3.
        b.emit(Opcode::Ldar, &[Operand::Reg(2)]).unwrap();
        b.emit(Opcode::Add, &[Operand::Reg(3)]).unwrap();
        b.emit(Opcode::Return, &[]).unwrap();
        let v2 = b.finish().unwrap();

        let layout = FrameLayout::new(0, 0, 4, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");

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
        callee_b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::Add, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::Return, &[]).unwrap();
        let callee_bc = callee_b.finish().unwrap();
        let callee_layout = FrameLayout::new(0, 1, 0, 0).unwrap();
        let callee = Function::with_empty_tables(Some("double"), callee_layout, callee_bc);

        // Caller: r0 = closure, r1 = 10, r2 = undefined (receiver);
        //         TailCall(r0, r2, [r1]).
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(10)]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        caller_b.emit(Opcode::LdaUndefined, &[]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        caller_b
            .emit(
                Opcode::TailCall,
                &[
                    Operand::Reg(0),
                    Operand::Reg(2),
                    Operand::RegList { base: 1, count: 1 },
                ],
            )
            .unwrap();
        // Dead code — TailCall should have replaced our frame.
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(-1)]).unwrap();
        caller_b.emit(Opcode::Return, &[]).unwrap();
        let caller_bc = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 3, 0).unwrap();
        let caller = Function::with_empty_tables(Some("main"), caller_layout, caller_bc);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");
        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle =
            runtime.alloc_closure(FunctionIndex(1), Vec::new(), ObjClosureFlags::default());
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter.execute_with_runtime(
            &module,
            FunctionIndex(0),
            &preseed,
            &mut runtime,
        ) {
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
        builder.emit(Opcode::LdaSmi, &[Operand::Imm(42)]).unwrap();
        builder.emit(Opcode::AssertConstructor, &[]).unwrap();
        builder.emit(Opcode::Return, &[]).unwrap();
        let v2 = builder.finish().unwrap();
        let layout = FrameLayout::new(0, 0, 0, 0).unwrap();
        let function = Function::with_empty_tables(Some("t"), layout, v2);
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0)).expect("module");
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
    // Ignored in M0 — exercises `allocate_construct_receiver` /
    // `apply_construct_return_override`, which are placeholders until
    // the real host-runtime helpers are restored. See
    // `interpreter::host_runtime` for the stub contract.
    #[test]
    #[ignore]
    fn construct_preseeded_closure_returns_receiver() {
        use crate::bigint::BigIntTable;
        use crate::call::CallTable;
        use crate::closure::ClosureTable;
        use crate::float::FloatTable;
        use crate::module::{FunctionSideTables, FunctionTables};
        use crate::object::ClosureFlags as ObjClosureFlags;
        use crate::property::PropertyNameTable;
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
        callee_b.emit(Opcode::Ldar, &[Operand::Reg(0)]).unwrap();
        callee_b.emit(Opcode::MulSmi, &[Operand::Imm(2)]).unwrap();
        callee_b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        callee_b.emit(Opcode::LdaThis, &[]).unwrap();
        callee_b.emit(Opcode::Star, &[Operand::Reg(2)]).unwrap();
        callee_b.emit(Opcode::Ldar, &[Operand::Reg(1)]).unwrap();
        callee_b
            .emit(
                Opcode::StaNamedProperty,
                &[Operand::Reg(2), Operand::Idx(0)],
            )
            .unwrap();
        callee_b.emit(Opcode::LdaUndefined, &[]).unwrap();
        callee_b.emit(Opcode::Return, &[]).unwrap();
        let callee_bc = callee_b.finish().unwrap();
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
        let callee = Function::new(Some("F"), callee_layout, callee_bc, callee_tables);

        // Caller: return new F(7)
        //   r0 = <closure F>      (preseeded)
        //   r1 = 7
        //   acc = Construct(r0, new_target=r0, [r1])
        //   return acc
        let mut caller_b = BytecodeBuilder::new();
        caller_b.emit(Opcode::LdaSmi, &[Operand::Imm(7)]).unwrap();
        caller_b.emit(Opcode::Star, &[Operand::Reg(1)]).unwrap();
        caller_b
            .emit(
                Opcode::Construct,
                &[
                    Operand::Reg(0),
                    Operand::Reg(0),
                    Operand::RegList { base: 1, count: 1 },
                ],
            )
            .unwrap();
        caller_b.emit(Opcode::Return, &[]).unwrap();
        let caller_bc = caller_b.finish().unwrap();
        let caller_layout = FrameLayout::new(0, 0, 2, 0).unwrap();
        let caller = Function::with_empty_tables(Some("main"), caller_layout, caller_bc);

        let module =
            Module::new(Some("m"), vec![caller, callee], FunctionIndex(0)).expect("module");

        let mut runtime = RuntimeState::new();
        let _ = runtime.enter_module(&module);
        let closure_handle =
            runtime.alloc_closure(FunctionIndex(1), Vec::new(), ObjClosureFlags::default());
        let preseed = [RegisterValue::from_object_handle(closure_handle.0)];

        let interpreter = Interpreter::new();
        let result = match interpreter.execute_with_runtime(
            &module,
            FunctionIndex(0),
            &preseed,
            &mut runtime,
        ) {
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
        callee_b.emit(Opcode::LdaSmi, &[Operand::Imm(7)]).unwrap();
        callee_b.emit(Opcode::Throw, &[]).unwrap();
        let callee_bc = callee_b.finish().unwrap();
        let callee = Function::with_empty_tables(
            Some("bomb"),
            FrameLayout::new(0, 0, 0, 0).unwrap(),
            callee_bc,
        );

        // Caller (fn_index 0): return CallDirect(1, [])
        let mut caller_b = BytecodeBuilder::new();
        caller_b
            .emit(
                Opcode::CallDirect,
                &[Operand::Idx(1), Operand::RegList { base: 0, count: 0 }],
            )
            .unwrap();
        caller_b.emit(Opcode::Return, &[]).unwrap();
        let caller_bc = caller_b.finish().unwrap();
        let caller = Function::with_empty_tables(
            Some("main"),
            FrameLayout::new(0, 0, 0, 0).unwrap(),
            caller_bc,
        );

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
