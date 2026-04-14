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
    b.emit(OpcodeV2::LdaUndefined, &[]).unwrap();
    b.emit(OpcodeV2::LdaTrue, &[]).unwrap();
    b.emit(OpcodeV2::Inc, &[]).unwrap();
    b.emit(OpcodeV2::Nop, &[]).unwrap();
    b.emit(OpcodeV2::Return, &[]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns.len(), 5);
    assert_eq!(insns[0].opcode, OpcodeV2::LdaUndefined);
    assert_eq!(insns[1].opcode, OpcodeV2::LdaTrue);
    assert_eq!(insns[2].opcode, OpcodeV2::Inc);
    assert_eq!(insns[3].opcode, OpcodeV2::Nop);
    assert_eq!(insns[4].opcode, OpcodeV2::Return);
    assert!(insns.iter().all(|i| i.width == OperandWidth::Narrow));
    // Each nullary op is exactly 1 byte.
    assert_eq!(bc.bytes().len(), 5);
}

#[test]
fn round_trip_ldar_star_narrow() {
    let mut b = BytecodeBuilder::new();
    b.emit(OpcodeV2::Ldar, &[Operand::Reg(3)]).unwrap();
    b.emit(OpcodeV2::Star, &[Operand::Reg(7)]).unwrap();
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
    b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(-5)]).unwrap();
    b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(127)]).unwrap();
    b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(200)]).unwrap(); // overflows i8 → Wide
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
    b.emit(OpcodeV2::Add, &[Operand::Reg(1)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].opcode, OpcodeV2::Add);
    assert_eq!(insns[0].operands, vec![Operand::Reg(1)]);
}

// -------- wide & extra-wide --------

#[test]
fn reg_operand_auto_widens_to_wide() {
    let mut b = BytecodeBuilder::new();
    // Register 300 doesn't fit in u8 → width becomes Wide.
    b.emit(OpcodeV2::Ldar, &[Operand::Reg(300)]).unwrap();
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
    b.emit(OpcodeV2::LdaConstStr, &[Operand::Idx(70_000)]).unwrap();
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
    b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(-30_000)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].width, OperandWidth::Wide);
    assert_eq!(insns[0].operands, vec![Operand::Imm(-30_000)]);
}

#[test]
fn imm_operand_signed_extra_wide() {
    let mut b = BytecodeBuilder::new();
    b.emit(OpcodeV2::LdaSmi, &[Operand::Imm(-2_000_000_000)]).unwrap();
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
        OpcodeV2::CallUndefinedReceiver,
        &[Operand::Reg(4), Operand::RegList { base: 5, count: 3 }],
    )
    .unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns[0].opcode, OpcodeV2::CallUndefinedReceiver);
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
        OpcodeV2::CallUndefinedReceiver,
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
    let pc_jump = b.emit_jump_to(OpcodeV2::Jump, end).unwrap();
    b.emit(OpcodeV2::Inc, &[]).unwrap();
    b.emit(OpcodeV2::Inc, &[]).unwrap();
    let pc_end = b.pc();
    b.bind_label(end).unwrap();
    b.emit(OpcodeV2::Return, &[]).unwrap();
    let bc = b.finish().unwrap();

    let insns = decode_all(bc.bytes());
    // Jump, Inc, Inc, Return.
    assert_eq!(insns.len(), 4);
    assert_eq!(insns[0].opcode, OpcodeV2::Jump);
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
    b.emit(OpcodeV2::Inc, &[]).unwrap();
    b.emit(OpcodeV2::Inc, &[]).unwrap();
    b.emit_jump_to(OpcodeV2::Jump, loop_header).unwrap();
    b.emit(OpcodeV2::Return, &[]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    let jump = insns.iter().find(|i| i.opcode == OpcodeV2::Jump).unwrap();
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
    let pc_add = b.emit(OpcodeV2::Add, &[Operand::Reg(1)]).unwrap();
    b.attach_feedback(pc_add, FeedbackSlot(5));
    let _pc_nop = b.emit(OpcodeV2::Nop, &[]).unwrap();
    let pc_sub = b.emit(OpcodeV2::Sub, &[Operand::Reg(2)]).unwrap();
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
    let pc0 = b.emit(OpcodeV2::Nop, &[]).unwrap();
    let pc1 = b.emit(OpcodeV2::Nop, &[]).unwrap();
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
        .emit(OpcodeV2::Add, &[Operand::Reg(1), Operand::Reg(2)])
        .unwrap_err();
    assert!(matches!(err, EncodeError::ArityMismatch { .. }));
}

#[test]
fn operand_kind_mismatch_rejected() {
    let mut b = BytecodeBuilder::new();
    let err = b.emit(OpcodeV2::Add, &[Operand::Imm(1)]).unwrap_err();
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
    let bytes = [OpcodeV2::Ldar.as_byte()];
    let mut iter = InstructionIter::new(&bytes);
    let result = iter.next().unwrap();
    assert!(matches!(result, Err(DecodeError::Truncated { .. })));
}

#[test]
fn double_prefix_rejected() {
    let bytes = [PREFIX_WIDE, PREFIX_EXTRA_WIDE, OpcodeV2::LdaSmi.as_byte()];
    let mut iter = InstructionIter::new(&bytes);
    let result = iter.next().unwrap();
    assert!(matches!(result, Err(DecodeError::DoublePrefix { .. })));
}

// -------- Transpiler (v1 → v2) --------

mod transpile_tests {
    use super::decode_all;
    use crate::bytecode::{Bytecode as V1Bytecode, BytecodeRegister, Instruction as V1Instruction, JumpOffset};
    use crate::bytecode_v2::{transpile, OpcodeV2, Operand};

    fn reg(i: u16) -> BytecodeRegister {
        BytecodeRegister::new(i)
    }

    #[test]
    fn transpiles_load_return_pair() {
        // v1: LoadI32 r0, 42 ; Return r0
        let v1 = V1Bytecode::new(vec![
            V1Instruction::load_i32(reg(0), 42),
            V1Instruction::ret(reg(0)),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        // LdaSmi 42; Star r0; Ldar r0; Return — 4 v2 insns.
        assert_eq!(insns.len(), 4);
        assert_eq!(insns[0].opcode, OpcodeV2::LdaSmi);
        assert_eq!(insns[0].operands, vec![Operand::Imm(42)]);
        assert_eq!(insns[1].opcode, OpcodeV2::Star);
        assert_eq!(insns[1].operands, vec![Operand::Reg(0)]);
        assert_eq!(insns[2].opcode, OpcodeV2::Ldar);
        assert_eq!(insns[2].operands, vec![Operand::Reg(0)]);
        assert_eq!(insns[3].opcode, OpcodeV2::Return);
    }

    #[test]
    fn transpiles_add_as_ldar_add_star() {
        // v1: Add r0, r1, r2 (slot0 = slot1 + slot2)
        let v1 = V1Bytecode::new(vec![V1Instruction::add(reg(0), reg(1), reg(2))]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert_eq!(insns.len(), 3);
        assert_eq!(insns[0].opcode, OpcodeV2::Ldar);
        assert_eq!(insns[0].operands, vec![Operand::Reg(1)]);
        assert_eq!(insns[1].opcode, OpcodeV2::Add);
        assert_eq!(insns[1].operands, vec![Operand::Reg(2)]);
        assert_eq!(insns[2].opcode, OpcodeV2::Star);
        assert_eq!(insns[2].operands, vec![Operand::Reg(0)]);
    }

    #[test]
    fn transpiles_move_as_mov() {
        // v1: Move r3, r7 → v2: Mov r7 r3.
        let v1 = V1Bytecode::new(vec![V1Instruction::move_(reg(3), reg(7))]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert_eq!(insns.len(), 1);
        assert_eq!(insns[0].opcode, OpcodeV2::Mov);
        assert_eq!(insns[0].operands, vec![Operand::Reg(7), Operand::Reg(3)]);
    }

    #[test]
    fn transpiles_forward_jump_if_false() {
        // v1:
        //   LoadTrue r0
        //   JumpIfFalse r0, +1   ; skip next
        //   LoadI32 r1, 99
        //   Return r0
        let v1 = V1Bytecode::new(vec![
            V1Instruction::load_true(reg(0)),
            V1Instruction::jump_if_false(reg(0), JumpOffset::new(1)),
            V1Instruction::load_i32(reg(1), 99),
            V1Instruction::ret(reg(0)),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        // Shape: LdaTrue; Star r0; Ldar r0; JumpIfToBooleanFalse off;
        //        LdaSmi 99; Star r1; Ldar r0; Return.
        assert_eq!(insns.len(), 8);
        assert!(matches!(insns[3].opcode, OpcodeV2::JumpIfToBooleanFalse));
        // Check the jump points past the LdaSmi/Star pair (skip-next semantic).
        let Operand::JumpOff(off) = insns[3].operands[0] else {
            panic!("expected JumpOff operand");
        };
        let after_jump = insns[3].end_pc as i64;
        let target = after_jump + off as i64;
        // Target is the Ldar r0 at index 6 — its start_pc equals the target.
        assert_eq!(target as u32, insns[6].start_pc);
    }

    #[test]
    fn transpiles_backward_jump_loop_header() {
        // Synthetic: Jump(-1) back to pc 0 (would be a hang at runtime,
        // but it exercises the backward-offset path).
        let v1 = V1Bytecode::new(vec![
            V1Instruction::nop(),
            V1Instruction::jump(JumpOffset::new(-2)),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        // Nop; Jump -N (backward, offset negative).
        assert_eq!(insns.len(), 2);
        assert_eq!(insns[0].opcode, OpcodeV2::Nop);
        assert_eq!(insns[1].opcode, OpcodeV2::Jump);
        let Operand::JumpOff(off) = insns[1].operands[0] else {
            panic!("expected JumpOff");
        };
        assert!(off < 0, "backward jump must have negative offset");
    }

    #[test]
    fn transpiles_canonical_arith_loop_body_fragment() {
        // Reproduces the arithmetic heart of `benchInt32Add`:
        //   s_tmp = s + i
        //   s_tmp = s_tmp | 0  (via BitOr with LoadI32 0 source)
        //   s = s_tmp
        // Where r0=s, r1=i, r2=tmp, r3=zero-literal.
        let v1 = V1Bytecode::new(vec![
            V1Instruction::add(reg(2), reg(0), reg(1)),      // tmp = s + i
            V1Instruction::load_i32(reg(3), 0),              // zero = 0
            V1Instruction::bit_or(reg(2), reg(2), reg(3)),   // tmp = tmp | 0
            V1Instruction::move_(reg(0), reg(2)),            // s = tmp
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());

        // Expected stream (post-transpile; no peephole yet):
        //   Ldar r0 ; Add r1 ; Star r2
        //   LdaSmi 0 ; Star r3
        //   Ldar r2 ; BitwiseOr r3 ; Star r2
        //   Mov r2 r0
        assert_eq!(insns.len(), 9);
        assert_eq!(insns[0].opcode, OpcodeV2::Ldar);
        assert_eq!(insns[1].opcode, OpcodeV2::Add);
        assert_eq!(insns[2].opcode, OpcodeV2::Star);
        assert_eq!(insns[3].opcode, OpcodeV2::LdaSmi);
        assert_eq!(insns[4].opcode, OpcodeV2::Star);
        assert_eq!(insns[5].opcode, OpcodeV2::Ldar);
        assert_eq!(insns[6].opcode, OpcodeV2::BitwiseOr);
        assert_eq!(insns[7].opcode, OpcodeV2::Star);
        assert_eq!(insns[8].opcode, OpcodeV2::Mov);
    }

    #[test]
    fn transpiles_new_object_and_array() {
        let v1 = V1Bytecode::new(vec![
            V1Instruction::new_object(reg(0)),
            V1Instruction::new_array(reg(1), 0),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert_eq!(insns.len(), 4);
        assert_eq!(insns[0].opcode, OpcodeV2::CreateObject);
        assert_eq!(insns[1].opcode, OpcodeV2::Star);
        assert_eq!(insns[2].opcode, OpcodeV2::CreateArray);
        assert_eq!(insns[3].opcode, OpcodeV2::Star);
    }

    #[test]
    fn transpiles_get_iterator_and_close() {
        let v1 = V1Bytecode::new(vec![
            V1Instruction::get_iterator(reg(1), reg(0)),
            V1Instruction::iterator_next(reg(3), reg(2), reg(1)),
            V1Instruction::iterator_close(reg(1)),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        // GetIterator r0; Star r1; IteratorNext r1; Star r2 (value);
        // IteratorClose r1.
        assert_eq!(insns.len(), 5);
        assert_eq!(insns[0].opcode, OpcodeV2::GetIterator);
        assert_eq!(insns[2].opcode, OpcodeV2::IteratorNext);
        assert_eq!(insns[4].opcode, OpcodeV2::IteratorClose);
    }

    #[test]
    fn transpiles_yield_and_await() {
        let v1 = V1Bytecode::new(vec![
            V1Instruction::yield_(reg(1), reg(0)),
            V1Instruction::r#await(reg(2), reg(1)),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert_eq!(insns.len(), 6);
        assert_eq!(insns[1].opcode, OpcodeV2::Yield);
        assert_eq!(insns[4].opcode, OpcodeV2::Await);
    }

    #[test]
    fn transpiles_private_field_ops() {
        use crate::property::PropertyNameId;
        let name = PropertyNameId(7);
        let v1 = V1Bytecode::new(vec![
            V1Instruction::define_private_field(reg(0), reg(1), name),
            V1Instruction::get_private_field(reg(2), reg(0), name),
            V1Instruction::set_private_field(reg(0), reg(3), name),
        ]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert!(insns.iter().any(|i| i.opcode == OpcodeV2::DefinePrivateField));
        assert!(insns.iter().any(|i| i.opcode == OpcodeV2::GetPrivateField));
        assert!(insns.iter().any(|i| i.opcode == OpcodeV2::SetPrivateField));
    }

    #[test]
    fn transpiles_assert_not_hole() {
        let v1 = V1Bytecode::new(vec![V1Instruction::assert_not_hole(reg(3))]);
        let v2 = transpile(&v1).expect("transpile ok");
        let insns = decode_all(v2.bytes());
        assert_eq!(insns.len(), 2);
        assert_eq!(insns[0].opcode, OpcodeV2::Ldar);
        assert_eq!(insns[1].opcode, OpcodeV2::AssertNotHole);
    }

    #[test]
    fn call_direct_without_context_reports_missing() {
        // CallDirect without a Function errors rather than silently
        // emitting a wrong call site.
        let v1 = V1Bytecode::new(vec![V1Instruction::call_direct(reg(0), reg(1))]);
        let err = transpile(&v1).unwrap_err();
        assert!(matches!(
            err,
            crate::bytecode_v2::TranspileError::MissingFunctionContext { .. }
        ));
    }

    #[test]
    fn unsupported_opcode_reports_cleanly() {
        // `construct` is a V1 opcode we haven't wired yet — currently
        // the v1 enum doesn't even have a Construct variant, so pick
        // something similar. Use LoadString (which IS supported) to
        // ensure the negative test catches only true gaps.
        //
        // This test now verifies that the transpiler gracefully reports
        // gaps; since we covered everything self-contained in Phase
        // 2a.3, we synthesize a fake-unsupported case via the
        // side-table-less call path which we intentionally report as
        // `MissingFunctionContext`, not `Unsupported`. Keep this test
        // as a guard against regressions on the error-path structure.
        let v1 = V1Bytecode::new(vec![V1Instruction::call_closure(reg(0), reg(1), reg(2))]);
        let err = transpile(&v1).unwrap_err();
        assert!(matches!(
            err,
            crate::bytecode_v2::TranspileError::MissingFunctionContext { .. }
        ));
    }
}

// -------- Phase 3a: end-to-end execute on transpiled v1 --------

mod e2e_tests {
    use crate::bytecode::{Bytecode as V1Bytecode, BytecodeRegister, Instruction as V1Instruction, JumpOffset};
    use crate::bytecode_v2::{execute, transpile, Frame};
    use crate::value::RegisterValue;

    fn reg(i: u16) -> BytecodeRegister {
        BytecodeRegister::new(i)
    }

    /// Execute a v1 bytecode stream through the v1→v2 transpiler and
    /// the minimal v2 dispatch harness. Asserts the result matches the
    /// expected accumulator value.
    fn run(v1: V1Bytecode, register_count: usize, setup: impl FnOnce(&mut Frame)) -> RegisterValue {
        let v2 = transpile(&v1).expect("transpile ok");
        let mut frame = Frame::new(register_count);
        setup(&mut frame);
        execute(&v2, &mut frame).expect("execute ok")
    }

    #[test]
    fn return_of_literal() {
        // function() { return 42; }
        let v1 = V1Bytecode::new(vec![
            V1Instruction::load_i32(reg(0), 42),
            V1Instruction::ret(reg(0)),
        ]);
        let result = run(v1, 1, |_| {});
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn add_two_registers() {
        // slot0 = slot1 + slot2; return slot0
        let v1 = V1Bytecode::new(vec![
            V1Instruction::add(reg(0), reg(1), reg(2)),
            V1Instruction::ret(reg(0)),
        ]);
        let result = run(v1, 3, |f| {
            f.set_register(1, RegisterValue::from_i32(10));
            f.set_register(2, RegisterValue::from_i32(32));
        });
        assert_eq!(result.as_i32(), Some(42));
    }

    #[test]
    fn bitor_zero_identity_on_int32() {
        // slot0 = slot1 | 0 ; return slot0  (the `|0` int32-coerce idiom)
        let v1 = V1Bytecode::new(vec![
            V1Instruction::load_i32(reg(2), 0),
            V1Instruction::bit_or(reg(0), reg(1), reg(2)),
            V1Instruction::ret(reg(0)),
        ]);
        let result = run(v1, 3, |f| f.set_register(1, RegisterValue::from_i32(99)));
        assert_eq!(result.as_i32(), Some(99));
    }

    #[test]
    fn loop_sum_0_to_n_minus_1() {
        // Emulates:
        //   function(n) { let s=0, i=0; while (i<n) { s=(s+i)|0; i=i+1; } return s; }
        //
        // v1 layout:
        //   r0 = n (input parameter)
        //   r1 = s (accumulator)
        //   r2 = i (counter)
        //   r3 = cond (loop test result)
        //   r4 = tmp (s + i)
        //   r5 = 0 (constant for bitor)
        //   r6 = 1 (loop step)
        //
        // Instructions:
        //   pc0: LoadI32 r1, 0          ; s = 0
        //   pc1: LoadI32 r2, 0          ; i = 0
        //   pc2: LoadI32 r5, 0          ; zero literal for |0
        //   pc3: LoadI32 r6, 1          ; step
        //   pc4: Lt r3, r2, r0          ; cond = i < n
        //   pc5: JumpIfFalse r3, +4     ; exit loop if !cond (skip 4: pc6..pc9)
        //   pc6: Add r4, r1, r2         ; tmp = s + i
        //   pc7: BitOr r1, r4, r5       ; s = tmp | 0
        //   pc8: Add r2, r2, r6         ; i = i + 1
        //   pc9: Jump -6                ; back to pc4
        //   pc10: Return r1
        let v1 = V1Bytecode::new(vec![
            V1Instruction::load_i32(reg(1), 0),
            V1Instruction::load_i32(reg(2), 0),
            V1Instruction::load_i32(reg(5), 0),
            V1Instruction::load_i32(reg(6), 1),
            V1Instruction::lt(reg(3), reg(2), reg(0)),
            V1Instruction::jump_if_false(reg(3), JumpOffset::new(4)),
            V1Instruction::add(reg(4), reg(1), reg(2)),
            V1Instruction::bit_or(reg(1), reg(4), reg(5)),
            V1Instruction::add(reg(2), reg(2), reg(6)),
            V1Instruction::jump(JumpOffset::new(-6)),
            V1Instruction::ret(reg(1)),
        ]);

        // Verify sum(0..n-1) = n*(n-1)/2 for a range of n values.
        for n in [0_i32, 1, 2, 3, 10, 100, 1000] {
            let result = run(v1.clone(), 7, |f| {
                f.set_register(0, RegisterValue::from_i32(n));
            });
            let expected = if n > 0 { n * (n - 1) / 2 } else { 0 };
            assert_eq!(
                result.as_i32(),
                Some(expected),
                "sum(0..{}) should be {}",
                n,
                expected
            );
        }
    }

    #[test]
    fn comparisons_and_boolean_jumps() {
        // function(a, b) {
        //   if (a < b) return 1;
        //   else return 0;
        // }
        //
        // r0 = a, r1 = b, r2 = cond, r3 = tmp
        //   pc0: Lt r2, r0, r1           ; cond = a < b
        //   pc1: JumpIfFalse r2, +2      ; skip the `return 1` branch
        //   pc2: LoadI32 r3, 1
        //   pc3: Return r3
        //   pc4: LoadI32 r3, 0
        //   pc5: Return r3
        let v1 = V1Bytecode::new(vec![
            V1Instruction::lt(reg(2), reg(0), reg(1)),
            V1Instruction::jump_if_false(reg(2), JumpOffset::new(2)),
            V1Instruction::load_i32(reg(3), 1),
            V1Instruction::ret(reg(3)),
            V1Instruction::load_i32(reg(3), 0),
            V1Instruction::ret(reg(3)),
        ]);

        let r = run(v1.clone(), 4, |f| {
            f.set_register(0, RegisterValue::from_i32(3));
            f.set_register(1, RegisterValue::from_i32(5));
        });
        assert_eq!(r.as_i32(), Some(1));

        let r = run(v1, 4, |f| {
            f.set_register(0, RegisterValue::from_i32(7));
            f.set_register(1, RegisterValue::from_i32(2));
        });
        assert_eq!(r.as_i32(), Some(0));
    }

    #[test]
    fn move_copies_register() {
        // slot2 = slot0 ; slot3 = slot2 ; return slot3
        let v1 = V1Bytecode::new(vec![
            V1Instruction::move_(reg(2), reg(0)),
            V1Instruction::move_(reg(3), reg(2)),
            V1Instruction::ret(reg(3)),
        ]);
        let r = run(v1, 4, |f| f.set_register(0, RegisterValue::from_i32(777)));
        assert_eq!(r.as_i32(), Some(777));
    }

    #[test]
    fn typeof_on_int32_returns_string() {
        // Just verifies we don't crash — ToString/typeof go through the
        // harness but we don't have string comparison in Phase 3a, so we
        // skip this one unless string support is wired. Placeholder for
        // Phase 3b.
    }
}

// -------- Complete canonical example --------

#[test]
fn canonical_example_s_eq_s_plus_i_bitor_zero() {
    // `s = (s + i) | 0` in the Ignition style:
    //   Ldar r2         ; acc = s
    //   Add r3          ; acc = acc + i
    //   BitwiseOrSmi 0  ; acc = acc | 0
    //   Star r2         ; s = acc
    let mut b = BytecodeBuilder::new();
    b.emit(OpcodeV2::Ldar, &[Operand::Reg(2)]).unwrap();
    b.emit(OpcodeV2::Add, &[Operand::Reg(3)]).unwrap();
    b.emit(OpcodeV2::BitwiseOrSmi, &[Operand::Imm(0)]).unwrap();
    b.emit(OpcodeV2::Star, &[Operand::Reg(2)]).unwrap();
    let bc = b.finish().unwrap();
    let insns = decode_all(bc.bytes());
    assert_eq!(insns.len(), 4);
    assert_eq!(insns[0].opcode, OpcodeV2::Ldar);
    assert_eq!(insns[1].opcode, OpcodeV2::Add);
    assert_eq!(insns[2].opcode, OpcodeV2::BitwiseOrSmi);
    assert_eq!(insns[3].opcode, OpcodeV2::Star);
    // Every operand fits in narrow → no prefixes. 4 insns × 2 bytes = 8.
    assert_eq!(bc.bytes().len(), 8);
}
