//! Unit tests for bytecode v2. Cover every operand kind, every operand
//! width (narrow / wide / extra-wide), prefix roll-forward, label
//! back-patching, feedback-slot attachment, and round-trip parity.

use super::*;

fn decode_all(bytes: &[u8]) -> Vec<Instruction> {
    InstructionIter::new(bytes)
        .collect::<Result<Vec<_>, _>>()
        .expect("round-trip decode failed")
}

// -------- narrow operand width --------

#[test]
fn round_trip_nullary_ops_narrow() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::LdaUndefined, &[]).unwrap();
    b.emit(Opcode::LdaTrue, &[]).unwrap();
    b.emit(Opcode::Inc, &[]).unwrap();
    b.emit(Opcode::Nop, &[]).unwrap();
    b.emit(Opcode::Return, &[]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns.len(), 5);
    assert_eq!(insns[0].opcode, Opcode::LdaUndefined);
    assert_eq!(insns[1].opcode, Opcode::LdaTrue);
    assert_eq!(insns[2].opcode, Opcode::Inc);
    assert_eq!(insns[3].opcode, Opcode::Nop);
    assert_eq!(insns[4].opcode, Opcode::Return);
    assert!(insns.iter().all(|i| i.width == OperandWidth::Narrow));
    // Each nullary op is exactly 1 byte.
    assert_eq!(bc.bytes().len(), 5);
}

#[test]
fn round_trip_ldar_star_narrow() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::Ldar, &[Operand::Reg(3)]).unwrap();
    b.emit(Opcode::Star, &[Operand::Reg(7)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns.len(), 2);
    assert_eq!(insns[0].operands, vec![Operand::Reg(3)]);
    assert_eq!(insns[1].operands, vec![Operand::Reg(7)]);
    // narrow Ldar/Star = 2 bytes each (opcode + 1B reg).
    assert_eq!(bc.bytes().len(), 4);
}

#[test]
fn round_trip_lda_smi_narrow_and_wide() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::LdaSmi, &[Operand::Imm(-5)]).unwrap();
    b.emit(Opcode::LdaSmi, &[Operand::Imm(127)]).unwrap();
    b.emit(Opcode::LdaSmi, &[Operand::Imm(200)]).unwrap(); // overflows i8 → Wide
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns.len(), 3);
    assert_eq!(insns[0].width, OperandWidth::Narrow);
    assert_eq!(insns[0].operands, vec![Operand::Imm(-5)]);
    assert_eq!(insns[1].width, OperandWidth::Narrow);
    assert_eq!(insns[1].operands, vec![Operand::Imm(127)]);
    assert_eq!(insns[2].width, OperandWidth::Wide);
    assert_eq!(insns[2].operands, vec![Operand::Imm(200)]);
}

#[test]
fn round_trip_add_reg() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::Add, &[Operand::Reg(1)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].opcode, Opcode::Add);
    assert_eq!(insns[0].operands, vec![Operand::Reg(1)]);
}

// -------- wide & extra-wide --------

#[test]
fn reg_operand_auto_widens_to_wide() {
    let mut b = BytecodeBuilder::new();
    // Register 300 doesn't fit in u8 → width becomes Wide.
    b.emit(Opcode::Ldar, &[Operand::Reg(300)]).unwrap();
    let bc = b.finish().unwrap();
    assert_eq!(bc.bytes()[0], PREFIX_WIDE);
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::Wide);
    assert_eq!(insns[0].operands, vec![Operand::Reg(300)]);
    // Prefix + opcode + 2 bytes reg = 4 bytes.
    assert_eq!(bc.bytes().len(), 4);
}

#[test]
fn idx_operand_auto_widens_to_extra_wide() {
    let mut b = BytecodeBuilder::new();
    // 70_000 doesn't fit in u16 → ExtraWide.
    b.emit(Opcode::LdaConstStr, &[Operand::Idx(70_000)])
        .unwrap();
    let bc = b.finish().unwrap();
    assert_eq!(bc.bytes()[0], PREFIX_EXTRA_WIDE);
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::ExtraWide);
    assert_eq!(insns[0].operands, vec![Operand::Idx(70_000)]);
    assert_eq!(bc.bytes().len(), 6);
}

#[test]
fn imm_operand_signed_wide() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::LdaSmi, &[Operand::Imm(-30_000)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::Wide);
    assert_eq!(insns[0].operands, vec![Operand::Imm(-30_000)]);
}

#[test]
fn imm_operand_signed_extra_wide() {
    let mut b = BytecodeBuilder::new();
    b.emit(Opcode::LdaSmi, &[Operand::Imm(-2_000_000_000)])
        .unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::ExtraWide);
    assert_eq!(insns[0].operands, vec![Operand::Imm(-2_000_000_000)]);
}

// -------- RegList --------

#[test]
fn reg_list_narrow() {
    let mut b = BytecodeBuilder::new();
    b.emit(
        Opcode::CallUndefinedReceiver,
        &[Operand::Reg(4), Operand::RegList { base: 5, count: 3 }],
    )
    .unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].opcode, Opcode::CallUndefinedReceiver);
    assert_eq!(
        insns[0].operands,
        vec![Operand::Reg(4), Operand::RegList { base: 5, count: 3 }]
    );
    // opcode + 1B reg + 1B base + 1B count = 4 bytes.
    assert_eq!(bc.bytes().len(), 4);
}

#[test]
fn reg_list_auto_widens_when_count_big() {
    let mut b = BytecodeBuilder::new();
    b.emit(
        Opcode::CallUndefinedReceiver,
        &[
            Operand::Reg(4),
            Operand::RegList {
                base: 5,
                count: 500,
            },
        ],
    )
    .unwrap();
    let bc = b.finish().unwrap();
    assert_eq!(bc.bytes()[0], PREFIX_WIDE);
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::Wide);
    assert_eq!(
        insns[0].operands,
        vec![
            Operand::Reg(4),
            Operand::RegList {
                base: 5,
                count: 500
            }
        ]
    );
}

// -------- Labels --------

#[test]
fn forward_jump_back_patched() {
    let mut b = BytecodeBuilder::new();
    let end = b.new_label();
    let pc_jump = b.emit_jump_to(Opcode::Jump, end).unwrap();
    b.emit(Opcode::Inc, &[]).unwrap();
    b.emit(Opcode::Inc, &[]).unwrap();
    let pc_end = b.pc();
    b.bind_label(end).unwrap();
    b.emit(Opcode::Return, &[]).unwrap();
    let bc = b.finish().unwrap();

    let insns = decode_all(bc.bytes());
    // Jump, Inc, Inc, Return.
    assert_eq!(insns.len(), 4);
    assert_eq!(insns[0].opcode, Opcode::Jump);
    let Operand::JumpOff(off) = insns[0].operands[0] else {
        panic!("expected JumpOff operand");
    };
    // Jump offset is measured from the byte after the jump instruction.
    let after_jump_pc = insns[0].end_pc;
    let target_pc = after_jump_pc as i64 + off as i64;
    assert_eq!(target_pc as u32, pc_end);
    let _ = pc_jump;
}

#[test]
fn backward_jump_resolved_immediately() {
    let mut b = BytecodeBuilder::new();
    let loop_header = b.new_label();
    b.bind_label(loop_header).unwrap();
    let loop_start_pc = b.pc();
    b.emit(Opcode::Inc, &[]).unwrap();
    b.emit(Opcode::Inc, &[]).unwrap();
    b.emit_jump_to(Opcode::Jump, loop_header).unwrap();
    b.emit(Opcode::Return, &[]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    let jump = insns.iter().find(|i| i.opcode == Opcode::Jump).unwrap();
    let Operand::JumpOff(off) = jump.operands[0] else {
        panic!("expected JumpOff operand");
    };
    let target = jump.end_pc as i64 + off as i64;
    assert_eq!(target as u32, loop_start_pc);
    assert!(off < 0, "backward jump must have negative offset");
}

// -------- Feedback map --------

#[test]
fn feedback_map_stores_and_retrieves_sparse_entries() {
    let mut b = BytecodeBuilder::new();
    let pc_add = b.emit(Opcode::Add, &[Operand::Reg(1)]).unwrap();
    b.attach_feedback(pc_add, FeedbackSlot(5));
    let _pc_nop = b.emit(Opcode::Nop, &[]).unwrap();
    let pc_sub = b.emit(Opcode::Sub, &[Operand::Reg(2)]).unwrap();
    b.attach_feedback(pc_sub, FeedbackSlot(9));
    let bc = b.finish().unwrap();
    assert_eq!(bc.feedback().get(pc_add), Some(FeedbackSlot(5)));
    assert_eq!(bc.feedback().get(pc_sub), Some(FeedbackSlot(9)));
    // PC with no slot returns None.
    assert_eq!(bc.feedback().get(pc_add + 1), None);
    assert_eq!(bc.feedback().len(), 2);
}

#[test]
fn feedback_entries_come_out_sorted() {
    let mut b = BytecodeBuilder::new();
    let pc0 = b.emit(Opcode::Nop, &[]).unwrap();
    let pc1 = b.emit(Opcode::Nop, &[]).unwrap();
    // Attach in reverse order on purpose — finish() sorts.
    b.attach_feedback(pc1, FeedbackSlot(2));
    b.attach_feedback(pc0, FeedbackSlot(1));
    let bc = b.finish().unwrap();
    let collected: Vec<_> = bc.feedback().iter().collect();
    assert_eq!(
        collected,
        vec![(pc0, FeedbackSlot(1)), (pc1, FeedbackSlot(2))]
    );
}

// -------- Error paths --------

#[test]
fn arity_mismatch_rejected() {
    let mut b = BytecodeBuilder::new();
    let err = b
        .emit(Opcode::Add, &[Operand::Reg(1), Operand::Reg(2)])
        .unwrap_err();
    assert!(matches!(err, EncodeError::ArityMismatch { .. }));
}

#[test]
fn operand_kind_mismatch_rejected() {
    let mut b = BytecodeBuilder::new();
    let err = b.emit(Opcode::Add, &[Operand::Imm(1)]).unwrap_err();
    assert!(matches!(err, EncodeError::OperandKindMismatch { .. }));
}

#[test]
fn unknown_opcode_byte_errors() {
    // 0xAA is in the gap between the highest defined opcode (ImportMeta =
    // 0x99) and the prefix bytes (0xFE/0xFF).
    let bytes = [0xAA_u8];
    let mut iter = InstructionIter::new(&bytes);
    let result = iter.next().unwrap();
    assert!(matches!(result, Err(DecodeError::UnknownOpcode { .. })));
}

#[test]
fn truncated_instruction_errors() {
    // Ldar expects one narrow operand; give it only the opcode byte.
    let bytes = [Opcode::Ldar.as_byte()];
    let mut iter = InstructionIter::new(&bytes);
    let result = iter.next().unwrap();
    assert!(matches!(result, Err(DecodeError::Truncated { .. })));
}

#[test]
fn double_prefix_rejected() {
    let bytes = [PREFIX_WIDE, PREFIX_EXTRA_WIDE, Opcode::LdaSmi.as_byte()];
    let mut iter = InstructionIter::new(&bytes);
    let result = iter.next().unwrap();
    assert!(matches!(result, Err(DecodeError::DoublePrefix { .. })));
}

// -------- Transpiler (v1 → v2) --------
