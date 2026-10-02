//! Escape proof for activation-local arguments reads after AST lowering.
//!
//! # Contents
//! - `ArgumentsReadPlan` identifies the one allocation and its scalar consumers.
//! - A forward data-flow analysis follows register/local copies through branches.
//!
//! # Invariants
//! - Every possible use of the arguments identity is checked, including aliases.
//! - A join with an ordinary value cannot become an activation argument read.
//! - Unsupported control flow or an escaping use retains ordinary construction.
//! - A closure-context operand still holds its placeholder register until
//!   `FunctionContext::finish_code` routes it to a fresh register, so it is
//!   never a use of the arguments identity.
//! - Logical PCs stay unchanged, preserving branches, spans and source metadata.
//!
//! # See also
//! - `functions` owns AST lowering and eligibility for implicit arguments.
//! - `otter_bytecode::opcode_schema` owns register roles, including local indices.

use std::collections::VecDeque;

use otter_bytecode::opcode_schema::{
    ControlFlow, RegisterAccess, SuccessorSpec, opcode_schema, operand_spec_at,
};
use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};

const ORDINARY: u8 = 1;
const ARGUMENTS: u8 = 2;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ArgumentsReadPlan {
    pub(crate) allocation_pc: u32,
    pub(crate) lengths: Vec<u32>,
    pub(crate) elements: Vec<u32>,
}

fn register(operand: Operand) -> Option<usize> {
    match operand {
        Operand::Register(reg) => Some(usize::from(reg)),
        Operand::Imm32(reg) => usize::try_from(reg).ok(),
        Operand::ConstIndex(_) => None,
    }
}

/// Prove that the implicit object is consumed only by copies, `.length` and
/// indexed reads. Eligibility for eval, parameter mapping, suspension and
/// exception handlers is checked by the function compiler before this pass.
pub(crate) fn analyze(
    code: &FunctionCodeBuilder,
    constants: &[Constant],
    registers: u16,
    closure_context_operands: &[(u32, usize)],
) -> Option<ArgumentsReadPlan> {
    let width = usize::from(registers);
    let count = code.len();
    // Bound cold compiler memory for pathological generated functions.
    if count == 0 || width == 0 || count.checked_mul(width)? > 1_048_576 {
        return None;
    }
    let mut allocation = None;
    let mut successors = Vec::with_capacity(count);
    for pc in 0..count {
        let op = code.op(pc as u32)?;
        if op == Op::CollectArguments && allocation.replace(pc as u32).is_some() {
            return None;
        }
        let schema = opcode_schema(op);
        // Resumption edges need their own transfer rules. Keeping the object
        // in these functions is conservative and preserves the oracle.
        if schema.control_flow == ControlFlow::Suspend || op == Op::Eval {
            return None;
        }
        let mut edges = Vec::with_capacity(2);
        for successor in schema.successor_shape.exact() {
            match successor {
                SuccessorSpec::Fallthrough if pc + 1 < count => edges.push(pc + 1),
                SuccessorSpec::RelativeTarget { operand_index, .. } => {
                    let Operand::Imm32(delta) = code.operand(pc as u32, *operand_index)? else {
                        return None;
                    };
                    let target = (pc as i64 + 1).checked_add(i64::from(delta))?;
                    let target = usize::try_from(target).ok()?;
                    if target >= count {
                        return None;
                    }
                    edges.push(target);
                }
                SuccessorSpec::Fallthrough | SuccessorSpec::FrameReturn => {}
            }
        }
        successors.push(edges);
    }
    let allocation_pc = allocation?;
    let mut incoming = vec![vec![0u8; width]; count];
    incoming[0].fill(ORDINARY);
    let mut queue = VecDeque::from([0usize]);
    let mut queued = vec![false; count];
    queued[0] = true;
    while let Some(pc) = queue.pop_front() {
        queued[pc] = false;
        let op = code.op(pc as u32)?;
        let mut output = incoming[pc].clone();
        let copy = match op {
            Op::LoadLocal => Some((0, 1)),
            Op::StoreLocal => Some((1, 0)),
            _ => None,
        };
        for index in 0..code.operand_count(pc as u32)? {
            if operand_spec_at(op, index)?.register_access == RegisterAccess::Write {
                let dst = register(code.operand(pc as u32, index)?)?;
                *output.get_mut(dst)? = ORDINARY;
            }
        }
        if op == Op::CollectArguments {
            let dst = register(code.operand(pc as u32, 0)?)?;
            *output.get_mut(dst)? = ARGUMENTS;
        } else if let Some((dst, src)) = copy {
            let dst = register(code.operand(pc as u32, dst)?)?;
            let src = register(code.operand(pc as u32, src)?)?;
            *output.get_mut(dst)? = *incoming[pc].get(src)?;
        }
        for &next in &successors[pc] {
            let mut changed = false;
            for (before, after) in incoming[next].iter_mut().zip(&output) {
                let joined = *before | after;
                changed |= joined != *before;
                *before = joined;
            }
            if changed && !queued[next] {
                queue.push_back(next);
                queued[next] = true;
            }
        }
    }
    let mut plan = ArgumentsReadPlan {
        allocation_pc,
        lengths: Vec::new(),
        elements: Vec::new(),
    };
    for (pc, state) in incoming.iter().enumerate() {
        if state.iter().all(|value| *value == 0) {
            continue;
        }
        let op = code.op(pc as u32)?;
        for index in 0..code.operand_count(pc as u32)? {
            if operand_spec_at(op, index)?.register_access != RegisterAccess::Read
                || closure_context_operands.contains(&(pc as u32, index))
            {
                continue;
            }
            let src = register(code.operand(pc as u32, index)?)?;
            let value = *state.get(src)?;
            if value & ARGUMENTS == 0 {
                continue;
            }
            match (op, index) {
                (Op::LoadLocal, 1) | (Op::StoreLocal, 0) => {}
                (Op::LoadProperty, 1) if value == ARGUMENTS => {
                    let Operand::ConstIndex(name) = code.operand(pc as u32, 2)? else {
                        return None;
                    };
                    if !matches!(constants.get(name as usize), Some(Constant::String { utf16 }) if utf16.iter().copied().eq("length".encode_utf16()))
                    {
                        return None;
                    }
                    plan.lengths.push(pc as u32);
                }
                (Op::LoadElement, 1) if value == ARGUMENTS => plan.elements.push(pc as u32),
                _ => return None,
            }
        }
    }
    Some(plan)
}

/// Apply an admitted plan in one pass without renumbering source PCs.
pub(crate) fn lower(code: &mut FunctionCodeBuilder, plan: &ArgumentsReadPlan) {
    let mut result = FunctionCodeBuilder::new();
    for pc in 0..code.len() as u32 {
        let op = code.op(pc).expect("compiler instruction");
        let operands: Vec<_> = (0..code.operand_count(pc).expect("operand count"))
            .map(|index| code.operand(pc, index).expect("compiler operand"))
            .collect();
        if pc == plan.allocation_pc {
            // Every use of the identity is either a dead copy or replaced
            // below. A normal tagged undefined keeps all register roots valid.
            result.push(Op::LoadUndefined, &operands[..1]);
        } else if plan.lengths.binary_search(&pc).is_ok() {
            result.push(Op::LoadArgumentsLength, &operands[..1]);
        } else if plan.elements.binary_search(&pc).is_ok() {
            result.push(Op::LoadArgumentsElement, &[operands[0], operands[2]]);
        } else {
            result.push(op, &operands);
        }
    }
    *code = result;
}

#[cfg(test)]
mod tests {
    use super::*;
    use Operand::{ConstIndex as K, Imm32 as I, Register as R};

    fn body(instructions: &[(Op, &[Operand])]) -> FunctionCodeBuilder {
        let mut code = FunctionCodeBuilder::new();
        for (op, operands) in instructions {
            code.push(*op, operands);
        }
        code
    }

    #[test]
    fn follows_local_aliases_through_a_loop_and_register_reuse() {
        let code = body(&[
            (Op::CollectArguments, &[R(0), R(0)]),
            (Op::StoreLocal, &[R(0), I(1)]),
            (Op::LoadLocal, &[R(2), I(1)]),
            (Op::LoadProperty, &[R(2), R(2), K(0)]),
            (Op::LoadLocal, &[R(3), I(1)]),
            (Op::LoadElement, &[R(3), R(3), R(4)]),
            (Op::JumpIfTrue, &[I(-3), R(5)]),
            (Op::ReturnValue, &[R(3)]),
        ]);
        assert_eq!(
            analyze(
                &code,
                &[Constant::String {
                    utf16: "length".encode_utf16().collect()
                }],
                6,
                &[]
            ),
            Some(ArgumentsReadPlan {
                allocation_pc: 0,
                lengths: vec![3],
                elements: vec![5],
            })
        );
    }

    #[test]
    fn a_join_with_an_ordinary_receiver_keeps_the_object() {
        let code = body(&[
            (Op::CollectArguments, &[R(0), R(0)]),
            (Op::JumpIfTrue, &[I(1), R(1)]),
            (Op::LoadNull, &[R(0)]),
            (Op::LoadProperty, &[R(2), R(0), K(0)]),
            (Op::ReturnValue, &[R(2)]),
        ]);
        assert!(
            analyze(
                &code,
                &[Constant::String {
                    utf16: "length".encode_utf16().collect()
                }],
                3,
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn calls_captures_and_argument_keys_escape() {
        for (op, operands) in [
            (Op::Call, vec![R(2), R(1), K(1), R(0)]),
            (Op::StoreContextSlot, vec![R(0), R(1), I(0)]),
            (Op::LoadElement, vec![R(2), R(0), R(0)]),
            (Op::ReturnValue, vec![R(0)]),
        ] {
            let code = body(&[(Op::CollectArguments, &[R(0), R(0)]), (op, &operands)]);
            assert!(analyze(&code, &[], 3, &[]).is_none(), "{op:?}");
        }
    }

    #[test]
    fn use_before_alias_initialization_is_not_a_virtual_receiver() {
        let code = body(&[
            (Op::CollectArguments, &[R(0), R(0)]),
            (Op::LoadProperty, &[R(2), R(1), K(0)]),
            (Op::StoreLocal, &[R(0), I(1)]),
            (Op::LoadProperty, &[R(2), R(1), K(0)]),
            (Op::ReturnValue, &[R(2)]),
        ]);
        let plan = analyze(
            &code,
            &[Constant::String {
                utf16: "length".encode_utf16().collect(),
            }],
            3,
            &[],
        )
        .unwrap();
        assert_eq!(plan.lengths, vec![3]);
    }

    #[test]
    fn closure_context_placeholder_is_not_an_arguments_use() {
        // The closure-context operand of `LoadContextSlot` still names the
        // placeholder `r0`, which here also holds the arguments identity.
        let code = body(&[
            (Op::CollectArguments, &[R(0), R(0)]),
            (Op::LoadContextSlot, &[R(1), R(0), I(0)]),
            (Op::LoadProperty, &[R(2), R(0), K(0)]),
            (Op::ReturnValue, &[R(2)]),
        ]);
        let constants = [Constant::String {
            utf16: "length".encode_utf16().collect(),
        }];
        assert!(analyze(&code, &constants, 3, &[]).is_none());
        let plan = analyze(&code, &constants, 3, &[(1, 1)]).unwrap();
        assert_eq!(plan.lengths, vec![2]);
    }
}
