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

use super::step_outcome::StepOutcome;
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
        _runtime: &mut RuntimeState,
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
                activation.set_accumulator(read_reg(activation, r)?);
            }
            OpcodeV2::Star => {
                let r = reg(&instr.operands, 0)?;
                write_reg(activation, r, activation.accumulator())?;
            }
            OpcodeV2::Mov => {
                let src = reg(&instr.operands, 0)?;
                let dst = reg(&instr.operands, 1)?;
                let v = read_reg(activation, src)?;
                write_reg(activation, dst, v)?;
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
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .add_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::Sub => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .sub_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::Mul => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                activation.set_accumulator(
                    activation
                        .accumulator()
                        .mul_i32(rhs)
                        .map_err(|_| InterpreterError::TypeError(Box::from("expected int32")))?,
                );
            }
            OpcodeV2::BitwiseOr => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l | r));
            }
            OpcodeV2::BitwiseAnd => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_i32(l & r));
            }
            OpcodeV2::BitwiseOrSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l | v));
            }
            OpcodeV2::AddSmi => {
                let v = imm(&instr.operands, 0)?;
                let l = i32_of(activation.accumulator())?;
                activation.set_accumulator(RegisterValue::from_i32(l.wrapping_add(v)));
            }

            // ---- Comparisons (int32 ordered) ----
            OpcodeV2::TestLessThan => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l < r));
            }
            OpcodeV2::TestGreaterThan => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l > r));
            }
            OpcodeV2::TestLessThanOrEqual => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l <= r));
            }
            OpcodeV2::TestGreaterThanOrEqual => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                let l = i32_of(activation.accumulator())?;
                let r = i32_of(rhs)?;
                activation.set_accumulator(RegisterValue::from_bool(l >= r));
            }
            OpcodeV2::TestEqualStrict => {
                let rhs = read_reg(activation, reg(&instr.operands, 0)?)?;
                activation.set_accumulator(RegisterValue::from_bool(
                    activation.accumulator() == rhs,
                ));
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

fn jump_off(ops: &[Operand], pos: usize) -> Result<i32, InterpreterError> {
    match ops.get(pos) {
        Some(Operand::JumpOff(v)) => Ok(*v),
        _ => Err(InterpreterError::NativeCall(
            Box::from("v2 operand kind mismatch: expected JumpOff"),
        )),
    }
}

fn read_reg(act: &Activation, index: RegisterIndex) -> Result<RegisterValue, InterpreterError> {
    act.register(index)
}

fn write_reg(
    act: &mut Activation,
    index: RegisterIndex,
    value: RegisterValue,
) -> Result<(), InterpreterError> {
    act.set_register(index, value)
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
}
