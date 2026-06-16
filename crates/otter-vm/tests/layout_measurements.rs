//! Runtime layout measurements for compact-VM refactors.
//!
//! # Contents
//! - Prints `size_of` / `align_of` for hot VM and bytecode structs.
//! - Prints spilled operand pressure for a representative fixed-width
//!   bytecode function.
//!
//! # Invariants
//! - This test is intentionally non-ratcheting: it provides a fast
//!   `--nocapture` measurement point without freezing unstable layouts.
//!
//! # See also
//! - `VM_REFACTOR_PLAN.md`

use std::mem::{align_of, size_of};

use otter_bytecode::{Function, Instruction, Op, Operand};
use otter_vm::{Frame, Value, VmError};

#[test]
fn print_hot_struct_layouts() {
    eprintln!(
        "otter_vm::Value: size={} align={}",
        size_of::<Value>(),
        align_of::<Value>()
    );
    eprintln!(
        "otter_vm::Frame: size={} align={}",
        size_of::<Frame>(),
        align_of::<Frame>()
    );
    eprintln!(
        "otter_vm::VmError: size={} align={}",
        size_of::<VmError>(),
        align_of::<VmError>()
    );
    eprintln!(
        "otter_bytecode::Instruction: size={} align={}",
        size_of::<Instruction>(),
        align_of::<Instruction>()
    );
    eprintln!(
        "otter_bytecode::Operand: size={} align={}",
        size_of::<Operand>(),
        align_of::<Operand>()
    );
}

#[test]
fn print_representative_bytecode_operand_pressure() {
    let function = representative_fixed_width_function();
    let instruction_stream_bytes = function.code.len() * size_of::<Instruction>();
    let operand_slots: usize = function.code.iter().map(|instr| instr.operands.len()).sum();
    let spilled_operand_instructions = function
        .code
        .iter()
        .filter(|instr| instr.operands.spilled_operand_len() > 0)
        .count();
    let spilled_operand_bytes: usize = function
        .code
        .iter()
        .map(|instr| instr.operands.spilled_operand_len() * size_of::<Operand>())
        .sum();

    eprintln!(
        "representative fixed-width function: instrs={} instruction_stream_bytes={} operand_slots={} spilled_operand_instructions={} spilled_operand_bytes={}",
        function.code.len(),
        instruction_stream_bytes,
        operand_slots,
        spilled_operand_instructions,
        spilled_operand_bytes
    );
}

fn representative_fixed_width_function() -> Function {
    use Operand::{ConstIndex, Imm32, Register};

    Function {
        id: 0,
        name: "layout-fixed-width".to_string(),
        span: (0, 0),
        locals: 4,
        scratch: 4,
        code: vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Register(0), Imm32(1)].into(),
            },
            Instruction {
                pc: 1,
                op: Op::LoadInt32,
                operands: vec![Register(1), Imm32(2)].into(),
            },
            Instruction {
                pc: 2,
                op: Op::Add,
                operands: vec![Register(2), Register(0), Register(1)].into(),
            },
            Instruction {
                pc: 3,
                op: Op::NewObject,
                operands: vec![Register(3)].into(),
            },
            Instruction {
                pc: 4,
                op: Op::StoreProperty,
                operands: vec![Register(3), ConstIndex(0), Register(2)].into(),
            },
            Instruction {
                pc: 5,
                op: Op::LoadProperty,
                operands: vec![Register(2), Register(3), ConstIndex(0)].into(),
            },
            Instruction {
                pc: 6,
                op: Op::ReturnValue,
                operands: vec![Register(2)].into(),
            },
        ],
        ..Function::default()
    }
}
