//! Loop detection and qualification for guard hoisting via loop versioning.
//!
//! Detects backward jumps in bytecode to identify loops, then checks whether
//! each loop qualifies for an optimized (unguarded) fast path where type
//! checks are hoisted to a pre-header block.

use otter_vm_bytecode::TypeFlags;
use otter_vm_bytecode::instruction::Instruction;

/// Maximum number of instructions in a loop body for versioning.
/// Larger loops produce too much code bloat from duplication.
const MAX_LOOP_BODY_SIZE: usize = 64;

/// Information about a detected loop.
#[derive(Debug)]
pub(crate) struct LoopInfo {
    /// PC of the loop header (target of the backward jump).
    pub header_pc: usize,
    /// PC of the backward jump instruction.
    pub back_edge_pc: usize,
    /// Whether this loop qualifies for the optimized (unguarded) fast path.
    pub qualifies: bool,
    /// Registers that must be type-checked in the pre-header.
    /// These are the union of all arithmetic/comparison input registers in the loop body.
    pub check_registers: Vec<u16>,
}

/// Detect loops and determine which qualify for guard hoisting.
///
/// A loop is identified by a backward `Jump` instruction (offset < 0).
/// A loop qualifies when:
/// 1. All arithmetic/comparison instructions in the loop have Int32 feedback
/// 2. No Call/CallMethod/Construct/CallSpread inside the loop body
/// 3. No TryStart/TryEnd spanning the loop
/// 4. No GetProp/SetProp/GetElem/SetElem inside the loop body
/// 5. Loop body ≤ 64 instructions
pub(crate) fn detect_loops(
    instructions: &[Instruction],
    feedback_snapshot: &[TypeFlags],
) -> Vec<LoopInfo> {
    let mut loops = Vec::new();

    for (pc, instruction) in instructions.iter().enumerate() {
        // Look for backward jumps — these form loop back-edges
        let offset = match instruction {
            Instruction::Jump { offset } => offset.offset(),
            _ => continue,
        };

        // Only backward jumps (offset < 0 or offset == 0 with self-loop)
        if offset >= 0 {
            continue;
        }

        let header_pc = (pc as i64 + offset as i64) as usize;
        if header_pc > pc {
            continue; // invalid
        }

        let body_size = pc - header_pc + 1;
        if body_size > MAX_LOOP_BODY_SIZE {
            loops.push(LoopInfo {
                header_pc,
                back_edge_pc: pc,
                qualifies: false,
                check_registers: Vec::new(),
            });
            continue;
        }

        let mut qualifies = true;
        let mut check_registers = Vec::new();

        for body_pc in header_pc..=pc {
            let inst = &instructions[body_pc];

            // Disqualify: calls can change types via side effects
            if is_call_instruction(inst) {
                qualifies = false;
                break;
            }

            // Disqualify: property access can trigger arbitrary JS (getters/setters/Proxy)
            if is_property_access(inst) {
                qualifies = false;
                break;
            }

            // Disqualify: try/catch changes exception handling semantics
            if is_try_instruction(inst) {
                qualifies = false;
                break;
            }

            // Check arithmetic/comparison instructions for Int32 feedback
            if let Some((feedback_idx, input_regs)) = arith_feedback_info(inst) {
                let is_int32 = feedback_snapshot
                    .get(feedback_idx as usize)
                    .map_or(false, |tf| tf.is_int32_only());
                if !is_int32 {
                    qualifies = false;
                    break;
                }
                for reg in input_regs {
                    if !check_registers.contains(&reg) {
                        check_registers.push(reg);
                    }
                }
            }

            // Inc/Dec don't have feedback_index but are always int32-guarded
            if let Some(reg) = inc_dec_input_reg(inst) {
                if !check_registers.contains(&reg) {
                    check_registers.push(reg);
                }
            }
        }

        // Must have at least one arithmetic op to benefit from versioning
        if check_registers.is_empty() {
            qualifies = false;
        }

        loops.push(LoopInfo {
            header_pc,
            back_edge_pc: pc,
            qualifies,
            check_registers,
        });
    }

    loops
}

/// Returns true if the instruction is a function call.
fn is_call_instruction(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::Call { .. }
            | Instruction::CallMethod { .. }
            | Instruction::Construct { .. }
            | Instruction::CallSpread { .. }
            | Instruction::CallWithReceiver { .. }
            | Instruction::CallMethodComputed { .. }
            | Instruction::CallMethodComputedSpread { .. }
            | Instruction::ConstructSpread { .. }
            | Instruction::TailCall { .. }
            | Instruction::CallSuper { .. }
            | Instruction::CallSuperForward { .. }
            | Instruction::CallSuperSpread { .. }
            | Instruction::CallEval { .. }
    )
}

/// Returns true if the instruction accesses object properties (can trigger arbitrary JS).
fn is_property_access(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::GetProp { .. }
            | Instruction::SetProp { .. }
            | Instruction::GetElem { .. }
            | Instruction::SetElem { .. }
            | Instruction::GetPropConst { .. }
            | Instruction::SetPropConst { .. }
            | Instruction::GetPropQuickened { .. }
            | Instruction::SetPropQuickened { .. }
            | Instruction::GetLocalProp { .. }
            | Instruction::DeleteProp { .. }
            | Instruction::DefineProperty { .. }
            | Instruction::GetGlobal { .. }
            | Instruction::SetGlobal { .. }
            | Instruction::GetSuperProp { .. }
    )
}

/// Returns true if the instruction is try/catch related.
fn is_try_instruction(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::TryStart { .. } | Instruction::TryEnd | Instruction::Catch { .. }
    )
}

/// For arithmetic/comparison instructions with a feedback_index, return
/// (feedback_index, input register indices).
fn arith_feedback_info(inst: &Instruction) -> Option<(u16, Vec<u16>)> {
    match inst {
        Instruction::Add {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::Sub {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::Mul {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::Div {
            lhs,
            rhs,
            feedback_index,
            ..
        } => Some((*feedback_index, vec![lhs.0, rhs.0])),

        // Quickened variants already have Int32 type
        Instruction::AddInt32 {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::SubInt32 {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::MulInt32 {
            lhs,
            rhs,
            feedback_index,
            ..
        }
        | Instruction::DivInt32 {
            lhs,
            rhs,
            feedback_index,
            ..
        } => Some((*feedback_index, vec![lhs.0, rhs.0])),

        // Comparisons (Lt, Le, Gt, Ge) don't have feedback_index in bytecode
        // but they are handled by the guarded numeric path, so we don't need
        // to check them for loop versioning — they work on any type

        _ => None,
    }
}

/// For Inc/Dec instructions, return the input register.
fn inc_dec_input_reg(inst: &Instruction) -> Option<u16> {
    match inst {
        Instruction::Inc { src, .. } | Instruction::Dec { src, .. } => Some(src.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::operand::{JumpOffset, Register};

    fn make_int32_feedback() -> TypeFlags {
        let mut tf = TypeFlags::default();
        tf.seen_int32 = true;
        tf
    }

    fn make_mixed_feedback() -> TypeFlags {
        let mut tf = TypeFlags::default();
        tf.seen_int32 = true;
        tf.seen_string = true;
        tf
    }

    #[test]
    fn detects_simple_loop() {
        // pc 0: LoadInt32 r0, 0
        // pc 1: LoadInt32 r1, 1000000
        // pc 2: Lt r2, r0, r1        (header)
        // pc 3: JumpIfFalse r2, +3
        // pc 4: Add r0, r0, r1       (feedback 0)
        // pc 5: Inc r0, r0
        // pc 6: Jump -4               (back to pc 2)
        let instructions = vec![
            Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            },
            Instruction::LoadInt32 {
                dst: Register(1),
                value: 1000000,
            },
            Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            },
            Instruction::JumpIfFalse {
                cond: Register(2),
                offset: JumpOffset(3),
            },
            Instruction::Add {
                dst: Register(0),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            },
            Instruction::Inc {
                dst: Register(0),
                src: Register(0),
            },
            Instruction::Jump {
                offset: JumpOffset(-4),
            },
        ];

        let feedback = vec![make_int32_feedback()];
        let loops = detect_loops(&instructions, &feedback);

        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header_pc, 2);
        assert_eq!(loops[0].back_edge_pc, 6);
        assert!(loops[0].qualifies);
        // r0 and r1 from Add, r0 from Inc
        assert!(loops[0].check_registers.contains(&0));
        assert!(loops[0].check_registers.contains(&1));
    }

    #[test]
    fn disqualifies_loop_with_call() {
        let instructions = vec![
            Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            },
            Instruction::Call {
                dst: Register(0),
                func: Register(1),
                argc: 0,
            },
            Instruction::Jump {
                offset: JumpOffset(-2),
            },
        ];

        let feedback = vec![];
        let loops = detect_loops(&instructions, &feedback);

        assert_eq!(loops.len(), 1);
        assert!(!loops[0].qualifies);
    }

    #[test]
    fn disqualifies_non_int32_feedback() {
        let instructions = vec![
            Instruction::Add {
                dst: Register(0),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            },
            Instruction::Jump {
                offset: JumpOffset(-1),
            },
        ];

        let feedback = vec![make_mixed_feedback()];
        let loops = detect_loops(&instructions, &feedback);

        assert_eq!(loops.len(), 1);
        assert!(!loops[0].qualifies);
    }

    #[test]
    fn no_loops_for_forward_jumps() {
        let instructions = vec![
            Instruction::Jump {
                offset: JumpOffset(2),
            },
            Instruction::Nop,
            Instruction::ReturnUndefined,
        ];

        let feedback = vec![];
        let loops = detect_loops(&instructions, &feedback);

        assert!(loops.is_empty());
    }
}
