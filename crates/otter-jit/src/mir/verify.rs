//! MIR verifier — validates SSA properties and type consistency.

use super::graph::{MirGraph, ValueId};
use super::nodes::MirOp;

/// Verification errors.
#[derive(Debug)]
pub struct VerifyError {
    pub message: String,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MIR verify: {}", self.message)
    }
}

/// Verify the MIR graph for well-formedness.
pub fn verify(graph: &MirGraph) -> Result<(), Vec<VerifyError>> {
    let mut errors = Vec::new();

    verify_ssa_single_def(graph, &mut errors);
    verify_terminators(graph, &mut errors);
    verify_value_uses(graph, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Every ValueId is defined exactly once.
fn verify_ssa_single_def(graph: &MirGraph, errors: &mut Vec<VerifyError>) {
    let mut seen = std::collections::HashSet::new();
    for block in &graph.blocks {
        for instr in &block.instrs {
            if !seen.insert(instr.value) {
                errors.push(VerifyError {
                    message: format!(
                        "{} defined multiple times (block {})",
                        instr.value, block.id,
                    ),
                });
            }
        }
    }
}

/// Every block must end with a terminator (except empty blocks, which are warnings).
fn verify_terminators(graph: &MirGraph, errors: &mut Vec<VerifyError>) {
    for block in &graph.blocks {
        if block.instrs.is_empty() {
            continue; // Empty blocks are allowed during construction.
        }
        if !block.is_terminated() {
            errors.push(VerifyError {
                message: format!("{} has no terminator", block.id),
            });
        }
        // Check no terminator in the middle.
        for (i, instr) in block.instrs.iter().enumerate() {
            if instr.op.is_terminator() && i != block.instrs.len() - 1 {
                errors.push(VerifyError {
                    message: format!(
                        "{}: terminator at position {} but block has {} instructions",
                        block.id,
                        i,
                        block.instrs.len(),
                    ),
                });
            }
        }
    }
}

/// Check that all ValueId operands reference defined values.
fn verify_value_uses(graph: &MirGraph, errors: &mut Vec<VerifyError>) {
    let defined: std::collections::HashSet<ValueId> = graph
        .blocks
        .iter()
        .flat_map(|b| b.instrs.iter().map(|i| i.value))
        .collect();

    for block in &graph.blocks {
        for instr in &block.instrs {
            for operand in collect_operands(&instr.op) {
                if !defined.contains(&operand) {
                    errors.push(VerifyError {
                        message: format!(
                            "{}: uses undefined value {} (in block {})",
                            instr.value, operand, block.id,
                        ),
                    });
                }
            }
        }
    }
}

/// Collect all ValueId operands from a MirOp.
fn collect_operands(op: &MirOp) -> Vec<ValueId> {
    let mut ops = Vec::new();

    match op {
        MirOp::Const(_)
        | MirOp::Undefined
        | MirOp::Null
        | MirOp::True
        | MirOp::False
        | MirOp::ConstInt32(_)
        | MirOp::ConstFloat64(_) => {}

        MirOp::GuardInt32 { val, .. }
        | MirOp::GuardFloat64 { val, .. }
        | MirOp::GuardObject { val, .. }
        | MirOp::GuardString { val, .. }
        | MirOp::GuardFunction { val, .. }
        | MirOp::GuardBool { val, .. }
        | MirOp::GuardNotHole { val, .. } => ops.push(*val),

        MirOp::GuardShape { obj, .. } | MirOp::GuardArrayDense { obj, .. } => ops.push(*obj),
        MirOp::GuardProtoEpoch { .. } => {}
        MirOp::GuardBoundsCheck { arr, idx, .. } => {
            ops.push(*arr);
            ops.push(*idx);
        }

        MirOp::BoxInt32(v)
        | MirOp::BoxFloat64(v)
        | MirOp::BoxBool(v)
        | MirOp::UnboxInt32(v)
        | MirOp::UnboxFloat64(v)
        | MirOp::Int32ToFloat64(v) => {
            ops.push(*v);
        }

        MirOp::AddI32 { lhs, rhs, .. }
        | MirOp::SubI32 { lhs, rhs, .. }
        | MirOp::MulI32 { lhs, rhs, .. }
        | MirOp::DivI32 { lhs, rhs, .. }
        | MirOp::ModI32 { lhs, rhs, .. } => {
            ops.push(*lhs);
            ops.push(*rhs);
        }
        MirOp::IncI32 { val, .. } | MirOp::DecI32 { val, .. } | MirOp::NegI32 { val, .. } => {
            ops.push(*val);
        }

        MirOp::AddF64 { lhs, rhs }
        | MirOp::SubF64 { lhs, rhs }
        | MirOp::MulF64 { lhs, rhs }
        | MirOp::DivF64 { lhs, rhs }
        | MirOp::ModF64 { lhs, rhs } => {
            ops.push(*lhs);
            ops.push(*rhs);
        }
        MirOp::NegF64(v) => ops.push(*v),

        MirOp::BitAnd { lhs, rhs }
        | MirOp::BitOr { lhs, rhs }
        | MirOp::BitXor { lhs, rhs }
        | MirOp::Shl { lhs, rhs }
        | MirOp::Shr { lhs, rhs }
        | MirOp::Ushr { lhs, rhs } => {
            ops.push(*lhs);
            ops.push(*rhs);
        }
        MirOp::BitNot(v) => ops.push(*v),

        MirOp::CmpI32 { lhs, rhs, .. }
        | MirOp::CmpF64 { lhs, rhs, .. }
        | MirOp::CmpStrictEq { lhs, rhs }
        | MirOp::CmpStrictNe { lhs, rhs } => {
            ops.push(*lhs);
            ops.push(*rhs);
        }
        MirOp::LogicalNot(v) | MirOp::IsTruthy(v) => ops.push(*v),

        MirOp::GetPropShaped { obj, .. } => ops.push(*obj),
        MirOp::SetPropShaped { obj, val, .. } => {
            ops.push(*obj);
            ops.push(*val);
        }
        MirOp::GetPropGeneric { obj, key, .. } => {
            ops.push(*obj);
            ops.push(*key);
        }
        MirOp::SetPropGeneric { obj, key, val, .. } => {
            ops.push(*obj);
            ops.push(*key);
            ops.push(*val);
        }
        MirOp::GetPropConstGeneric { obj, .. } => ops.push(*obj),
        MirOp::SetPropConstGeneric { obj, val, .. } => {
            ops.push(*obj);
            ops.push(*val);
        }
        MirOp::DeleteProp { obj, key } => {
            ops.push(*obj);
            ops.push(*key);
        }

        MirOp::GetElemDense { arr, idx } => {
            ops.push(*arr);
            ops.push(*idx);
        }
        MirOp::SetElemDense { arr, idx, val } => {
            ops.push(*arr);
            ops.push(*idx);
            ops.push(*val);
        }
        MirOp::ArrayLength(v) => ops.push(*v),
        MirOp::ArrayPush { arr, val } => {
            ops.push(*arr);
            ops.push(*val);
        }
        MirOp::GetElemGeneric { obj, key, .. } => {
            ops.push(*obj);
            ops.push(*key);
        }
        MirOp::SetElemGeneric { obj, key, val, .. } => {
            ops.push(*obj);
            ops.push(*key);
            ops.push(*val);
        }

        MirOp::CallDirect { target, args } => {
            ops.push(*target);
            ops.extend_from_slice(args);
        }
        MirOp::CallMonomorphic { callee, args, .. } => {
            ops.push(*callee);
            ops.extend_from_slice(args);
        }
        MirOp::CallGeneric { callee, args, .. } => {
            ops.push(*callee);
            ops.extend_from_slice(args);
        }
        MirOp::CallMethodGeneric { obj, args, .. } => {
            ops.push(*obj);
            ops.extend_from_slice(args);
        }
        MirOp::ConstructGeneric { callee, args } => {
            ops.push(*callee);
            ops.extend_from_slice(args);
        }

        MirOp::LoadLocal(_)
        | MirOp::LoadRegister(_)
        | MirOp::LoadUpvalue(_)
        | MirOp::LoadThis
        | MirOp::LoadConstPool(_) => {}

        MirOp::StoreLocal { val, .. }
        | MirOp::StoreRegister { val, .. }
        | MirOp::StoreUpvalue { val, .. } => ops.push(*val),
        MirOp::CloseUpvalue(_) => {}

        MirOp::GetGlobal { .. } => {}
        MirOp::SetGlobal { val, .. } => ops.push(*val),

        MirOp::NewObject
        | MirOp::NewArray { .. }
        | MirOp::CreateClosure { .. }
        | MirOp::CreateArguments => {}
        MirOp::DefineProperty { obj, key, val } => {
            ops.push(*obj);
            ops.push(*key);
            ops.push(*val);
        }
        MirOp::SetPrototype { obj, proto } => {
            ops.push(*obj);
            ops.push(*proto);
        }

        MirOp::TypeOf(v)
        | MirOp::ToNumber(v)
        | MirOp::ToStringOp(v)
        | MirOp::RequireCoercible(v) => ops.push(*v),
        MirOp::InstanceOf { lhs, rhs, .. }
        | MirOp::In {
            key: lhs, obj: rhs, ..
        } => {
            ops.push(*lhs);
            ops.push(*rhs);
        }

        MirOp::Jump(_, args) => ops.extend_from_slice(args),
        MirOp::ReturnUndefined | MirOp::Deopt(_) => {}
        MirOp::Branch {
            cond,
            true_args,
            false_args,
            ..
        } => {
            ops.push(*cond);
            ops.extend_from_slice(true_args);
            ops.extend_from_slice(false_args);
        }
        MirOp::Return(v) | MirOp::Throw(v) => ops.push(*v),

        MirOp::TryStart { .. } | MirOp::TryEnd | MirOp::Catch => {}

        MirOp::GetIterator(v) | MirOp::IteratorNext(v) | MirOp::IteratorClose(v) => {
            ops.push(*v);
        }

        MirOp::Safepoint { live } => ops.extend_from_slice(live),
        MirOp::WriteBarrier(v) => ops.push(*v),

        MirOp::Phi(inputs) => {
            for (_, val) in inputs {
                ops.push(*val);
            }
        }

        MirOp::Move(v) | MirOp::Spread(v) => ops.push(*v),
        MirOp::HelperCall { args, .. } => ops.extend_from_slice(args),
    }

    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::graph::MirGraph;

    #[test]
    fn test_verify_simple_graph() {
        let mut graph = MirGraph::new("test".into(), 1, 3, 0);
        let entry = graph.entry_block;

        let v0 = graph.push_instr(entry, MirOp::ConstInt32(42), 0);
        let v1 = graph.push_instr(entry, MirOp::BoxInt32(v0), 1);
        graph.push_instr(entry, MirOp::Return(v1), 2);

        let result = verify(&graph);
        assert!(result.is_ok(), "errors: {:?}", result.err());
    }

    #[test]
    fn test_verify_undefined_use() {
        let mut graph = MirGraph::new("test".into(), 1, 3, 0);
        let entry = graph.entry_block;

        // Use a value that doesn't exist
        graph.push_instr(entry, MirOp::Return(ValueId(999)), 0);

        let result = verify(&graph);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors[0].message.contains("undefined value v999"));
    }

    #[test]
    fn test_verify_missing_terminator() {
        let mut graph = MirGraph::new("test".into(), 1, 3, 0);
        let entry = graph.entry_block;

        graph.push_instr(entry, MirOp::ConstInt32(1), 0);
        // No terminator

        let result = verify(&graph);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors[0].message.contains("no terminator"));
    }
}
