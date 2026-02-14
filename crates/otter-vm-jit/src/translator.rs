//! Bytecode to Cranelift IR translation.
//!
//! All values are i64 in Cranelift. The baseline translator handles a small
//! instruction subset. When a runtime condition can't be handled (e.g.,
//! division by zero), the generated code returns `BAILOUT_SENTINEL` instead
//! of trapping, allowing the caller to re-execute in the interpreter.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{InstBuilder, StackSlotData, StackSlotKind, types};
use cranelift_frontend::FunctionBuilder;
use otter_vm_bytecode::Function;
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::operand::Register;

use crate::JitError;
use crate::bailout::BAILOUT_SENTINEL;

fn jump_target(pc: usize, offset: i32, instruction_count: usize) -> Result<usize, JitError> {
    let target = pc as i64 + offset as i64;
    if !(0..instruction_count as i64).contains(&target) {
        return Err(JitError::InvalidJumpTarget {
            pc,
            offset,
            instruction_count,
        });
    }
    Ok(target as usize)
}

fn unsupported(pc: usize, instruction: &Instruction) -> JitError {
    let debug = format!("{:?}", instruction);
    let opcode = debug.split([' ', '{', '(']).next().unwrap_or("unknown");
    JitError::UnsupportedInstruction {
        pc,
        opcode: opcode.to_string(),
    }
}

fn read_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
) -> cranelift_codegen::ir::Value {
    builder
        .ins()
        .stack_load(types::I64, slots[reg.index() as usize], 0)
}

fn write_reg(
    builder: &mut FunctionBuilder<'_>,
    slots: &[cranelift_codegen::ir::StackSlot],
    reg: Register,
    value: cranelift_codegen::ir::Value,
) {
    builder
        .ins()
        .stack_store(value, slots[reg.index() as usize], 0);
}

/// Emit a `return BAILOUT_SENTINEL` — signals the caller to re-execute
/// in the interpreter.
fn emit_bailout_return(builder: &mut FunctionBuilder<'_>) {
    let sentinel = builder.ins().iconst(types::I64, BAILOUT_SENTINEL);
    builder.ins().return_(&[sentinel]);
}

/// Translate a bytecode function into Cranelift IR.
///
/// Supported instruction subset:
/// - `LoadInt8`, `LoadInt32`, `Move`
/// - `Add`, `Sub`, `Mul`, `Div` (div bails out on divide-by-zero)
/// - `Jump`, `JumpIfTrue`, `JumpIfFalse`
/// - `Return`, `ReturnUndefined`, `Nop`
///
/// Unsupported instructions are rejected at compile time.
/// Runtime failures (e.g., div by zero) return `BAILOUT_SENTINEL`.
pub fn translate_function(
    builder: &mut FunctionBuilder<'_>,
    function: &Function,
) -> Result<(), JitError> {
    let instruction_count = function.instructions.len();
    if instruction_count == 0 {
        let zero = builder.ins().iconst(types::I64, 0);
        builder.ins().return_(&[zero]);
        return Ok(());
    }

    let reg_count = function.register_count as usize;
    let mut slots = Vec::with_capacity(reg_count);
    for _ in 0..reg_count {
        slots.push(builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            8,
        )));
    }

    let mut blocks = Vec::with_capacity(instruction_count);
    for _ in 0..instruction_count {
        blocks.push(builder.create_block());
    }

    let entry = builder.create_block();
    let exit = builder.create_block();

    builder.switch_to_block(entry);
    let zero = builder.ins().iconst(types::I64, 0);
    for idx in 0..reg_count {
        builder.ins().stack_store(zero, slots[idx], 0);
    }
    builder.ins().jump(blocks[0], &[]);

    for (pc, instruction) in function.instructions.iter().enumerate() {
        builder.switch_to_block(blocks[pc]);

        match instruction {
            Instruction::LoadInt8 { dst, value } => {
                let v = builder.ins().iconst(types::I64, i64::from(*value));
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::LoadInt32 { dst, value } => {
                let v = builder.ins().iconst(types::I64, i64::from(*value));
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::Move { dst, src } => {
                let v = read_reg(builder, &slots, *src);
                write_reg(builder, &slots, *dst, v);
            }
            Instruction::Add { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &slots, *lhs);
                let right = read_reg(builder, &slots, *rhs);
                let out = builder.ins().iadd(left, right);
                write_reg(builder, &slots, *dst, out);
            }
            Instruction::Sub { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &slots, *lhs);
                let right = read_reg(builder, &slots, *rhs);
                let out = builder.ins().isub(left, right);
                write_reg(builder, &slots, *dst, out);
            }
            Instruction::Mul { dst, lhs, rhs, .. } => {
                let left = read_reg(builder, &slots, *lhs);
                let right = read_reg(builder, &slots, *rhs);
                let out = builder.ins().imul(left, right);
                write_reg(builder, &slots, *dst, out);
            }
            Instruction::Div { dst, lhs, rhs, .. } => {
                // Division: bail out on divide-by-zero instead of trapping
                let left = read_reg(builder, &slots, *lhs);
                let right = read_reg(builder, &slots, *rhs);
                let rhs_nonzero = builder.ins().icmp_imm(IntCC::NotEqual, right, 0);
                let safe_block = builder.create_block();
                let bailout_block = builder.create_block();
                builder
                    .ins()
                    .brif(rhs_nonzero, safe_block, &[], bailout_block, &[]);

                // Bailout path: return sentinel
                builder.switch_to_block(bailout_block);
                emit_bailout_return(builder);

                // Safe path: perform division
                builder.switch_to_block(safe_block);
                let out = builder.ins().sdiv(left, right);
                write_reg(builder, &slots, *dst, out);
            }
            Instruction::Jump { offset } => {
                let target = jump_target(pc, offset.offset(), instruction_count)?;
                builder.ins().jump(blocks[target], &[]);
                continue;
            }
            Instruction::JumpIfTrue { cond, offset } => {
                let cond_val = read_reg(builder, &slots, *cond);
                let is_true = builder.ins().icmp_imm(IntCC::NotEqual, cond_val, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_true, blocks[jump_to], &[], blocks[fallthrough], &[]);
                } else {
                    builder.ins().brif(is_true, blocks[jump_to], &[], exit, &[]);
                }
                continue;
            }
            Instruction::JumpIfFalse { cond, offset } => {
                let cond_val = read_reg(builder, &slots, *cond);
                let is_false = builder.ins().icmp_imm(IntCC::Equal, cond_val, 0);
                let jump_to = jump_target(pc, offset.offset(), instruction_count)?;
                let fallthrough = pc + 1;
                if fallthrough < instruction_count {
                    builder
                        .ins()
                        .brif(is_false, blocks[jump_to], &[], blocks[fallthrough], &[]);
                } else {
                    builder
                        .ins()
                        .brif(is_false, blocks[jump_to], &[], exit, &[]);
                }
                continue;
            }
            Instruction::Return { src } => {
                let out = read_reg(builder, &slots, *src);
                builder.ins().return_(&[out]);
                continue;
            }
            Instruction::ReturnUndefined => {
                let zero = builder.ins().iconst(types::I64, 0);
                builder.ins().return_(&[zero]);
                continue;
            }
            Instruction::Nop => {}
            _ => return Err(unsupported(pc, instruction)),
        }

        let next_pc = pc + 1;
        if next_pc < instruction_count {
            builder.ins().jump(blocks[next_pc], &[]);
        } else {
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().return_(&[zero]);
        }
    }

    builder.switch_to_block(exit);
    let zero = builder.ins().iconst(types::I64, 0);
    builder.ins().return_(&[zero]);

    builder.seal_all_blocks();
    Ok(())
}
