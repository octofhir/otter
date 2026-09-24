//! Number representation for arithmetic whose consumers only need boxed values.
//!
//! # Contents
//! - Use-demand analysis for guarded tagged immediate arithmetic.
//! - Relaxation of an isolated Int32 decode and add/sub into Number operations.
//!
//! # Invariants
//! - Int32 feedback is not a permanent type proof for a mutable tagged value.
//! - Integer consumers, typed element indices, CFG joins and unboxed native
//!   arguments retain their representation. Proven Int32 element loads do too.
//! - A decode changes only when one immediate operation is its sole semantic
//!   user and every result consumer accepts Number boxing. Frame states derive
//!   their representation from the resulting nodes, never stale type copies.
//! - The Number guard remains at the original pre-effect decode state; only
//!   the unnecessary Int32 restriction and its overflow frame state disappear.
//!
//! # See also
//! - `hir` — authoritative node types and pre-operation frame states.
//! - `super::select_with_loop_entries` — allocation and boxing.

use super::hir::{
    NumericDirectCallArguments, NumericElementAccess, NumericFramePoint, NumericFunction,
    NumericNode, NumericTerminator, NumericType, NumericValue,
};
use otter_vm::{JitCompileSnapshot, JitElementRepr};

pub(super) fn relax(function: &mut NumericFunction, view: &JitCompileSnapshot) -> usize {
    let mut uses = vec![Vec::new(); function.nodes.len()];
    for (index, &node) in function.nodes.iter().enumerate() {
        if visit_inputs(function, node, |value, boxed| {
            if let Some(users) = uses.get_mut(value.0) {
                users.push((index, boxed));
            }
        })
        .is_none()
        {
            return 0;
        }
    }
    let mut owners = vec![None; function.nodes.len()];
    for (block_index, block) in function.blocks.iter().enumerate() {
        for value in &block.nodes {
            owners[value.0] = Some(block_index);
        }
        for value in block.successor_arguments.iter().flatten() {
            uses[value.0].push((usize::MAX, false));
        }
        match block.terminator {
            NumericTerminator::Branch { condition, .. } => {
                uses[condition.0].push((usize::MAX, false))
            }
            NumericTerminator::Return(value) | NumericTerminator::Throw(value) => {
                uses[value.0].push((usize::MAX, true))
            }
            NumericTerminator::Jump => {}
        }
    }
    let mut changed = 0;
    for index in 0..uses.len() {
        let (decode, immediate, add) = match function.nodes[index] {
            NumericNode::IntegerAddImmediate(source, immediate) => (source, immediate, true),
            NumericNode::IntegerSubImmediate(source, immediate) => (source, immediate, false),
            _ => continue,
        };
        let NumericNode::TaggedToInt32(source) = function.nodes[decode.0] else {
            continue;
        };
        if uses[decode.0] != [(index, false)]
            || uses[index].is_empty()
            || uses[index].iter().any(|(_, boxed)| !boxed)
        {
            continue;
        }
        if let NumericNode::ElementLoad { byte_pc, .. } = function.nodes[source.0]
            && view
                .element_accesses
                .get(&byte_pc)
                .is_some_and(|access| access.element == JitElementRepr::Int32)
        {
            continue;
        }
        let Some(block) = owners[index] else {
            continue;
        };
        if owners[decode.0] != Some(block) {
            continue;
        }
        let Some(position) = function.blocks[block]
            .nodes
            .iter()
            .position(|value| value.0 == index)
        else {
            continue;
        };
        let constant = NumericValue(function.nodes.len());
        function
            .nodes
            .push(NumericNode::Constant(f64::from(immediate)));
        function.blocks[block].nodes.insert(position, constant);
        function.nodes[decode.0] = NumericNode::TaggedToNumber(source);
        function.nodes[index] = if add {
            NumericNode::Add(decode, constant)
        } else {
            NumericNode::Sub(decode, constant)
        };
        changed += 1;
    }
    if changed != 0 {
        function.frame_states.retain(|state| match state.point {
            NumericFramePoint::Node(value) => {
                function.nodes[value.0].frame_state_purpose().is_some()
            }
            NumericFramePoint::Backedge { .. } => true,
        });
    }
    changed
}

fn span(
    function: &NumericFunction,
    start: u32,
    count: u32,
    boxed: bool,
    visit: &mut impl FnMut(NumericValue, bool),
) -> Option<()> {
    let start = usize::try_from(start).ok()?;
    let end = start.checked_add(usize::try_from(count).ok()?)?;
    for &value in function.operand_values.get(start..end)? {
        visit(value, boxed);
    }
    Some(())
}

/// Exhaustive use classification: adding a node requires stating its demands.
fn visit_inputs(
    function: &NumericFunction,
    node: NumericNode,
    mut visit: impl FnMut(NumericValue, bool),
) -> Option<()> {
    use NumericNode::*;
    match node {
        Parameter { .. }
        | BlockParameter(_)
        | TaggedConstant(_)
        | This
        | StringConstantCell { .. }
        | ColdCallExit { .. }
        | IntegerConstant(_)
        | BooleanConstant(_)
        | Constant(_) => {}
        Binding { inputs, .. } | CommittedValue { inputs, .. } => {
            for input in inputs.into_iter().flatten() {
                visit(input, true);
            }
        }
        ConstructorFieldStore {
            object: receiver,
            value,
            ..
        }
        | PropertyStore {
            receiver, value, ..
        } => {
            visit(receiver, true);
            visit(value, true);
        }
        PropertyLoad { receiver, .. } | PropertyShapeLoad { receiver, .. } => {
            visit(receiver, true);
        }
        BaseConstructResult { result, receiver } => {
            visit(result, true);
            visit(receiver, true);
        }
        ConstructReceiver { source, .. }
        | ConstructReceiverHit(source)
        | InlineConstructGuard { source, .. }
        | InlineCallGuard { source, .. }
        | InlineMethodGuard { source, .. }
        | BoxTagged(source) => visit(source, true),
        ElementLoad {
            receiver, index, ..
        } => {
            visit(receiver, true);
            visit(index, false);
        }
        ElementStore {
            receiver,
            index,
            value,
            access,
            ..
        } => {
            visit(receiver, true);
            visit(index, false);
            visit(value, access != Some(NumericElementAccess::PackedDouble));
        }
        LiteralAllocation {
            argument_start,
            argument_count,
            ..
        } => span(function, argument_start, argument_count, true, &mut visit)?,
        DirectCall {
            source, arguments, ..
        } => {
            visit(source, true);
            match arguments {
                NumericDirectCallArguments::Fixed { start, count } => {
                    span(function, start, count, true, &mut visit)?
                }
                NumericDirectCallArguments::Spread(value) => visit(value, false),
            }
        }
        NativeCall {
            receiver,
            target,
            argument_start,
            ..
        } => {
            visit(receiver, true);
            if let super::hir::NumericNativeCallTarget::Resolved { callee, .. } = target {
                visit(callee, true);
            }
            span(
                function,
                argument_start,
                u32::from(target.declaration().argument_count),
                false,
                &mut visit,
            )?;
        }
        NativeLeaf {
            source,
            target,
            value_type,
            argument_start,
            ..
        } => {
            visit(source, true);
            span(
                function,
                argument_start,
                u32::from(target.argument_count),
                value_type != NumericType::Int32,
                &mut visit,
            )?;
        }
        TaggedStrictEqual(left, right) => {
            visit(left, true);
            visit(right, true);
        }
        TaggedToNumber(value)
        | TaggedToInt32(value)
        | ClassSuperConstructor(value)
        | TaggedToBoolean(value)
        | TaggedNullishEqual { value, .. }
        | ArrayConstruct { length: value, .. }
        | WidenInt32(value)
        | WidenUint32(value)
        | FloatToInt32(value)
        | BooleanToInt32(value)
        | IntegerNeg(value)
        | IntegerNot(value)
        | Neg(value)
        | IntegerToBoolean(value)
        | FloatToBoolean(value)
        | BooleanNot(value)
        | IntegerAddImmediate(value, _)
        | IntegerSubImmediate(value, _)
        | IntegerAndImmediate(value, _)
        | IntegerLessThanImmediate(value, _)
        | IntegerEqualImmediate(value, _)
        | IntegerNotEqualImmediate(value, _) => visit(value, false),
        TaggedStringConcat(left, right)
        | IntegerAdd(left, right)
        | IntegerSub(left, right)
        | IntegerMul(left, right)
        | IntegerAnd(left, right)
        | IntegerOr(left, right)
        | IntegerXor(left, right)
        | IntegerShiftLeft(left, right)
        | IntegerShiftRight(left, right)
        | IntegerShiftRightLogical(left, right)
        | IntegerEqual(left, right)
        | IntegerNotEqual(left, right)
        | IntegerLessThan(left, right)
        | IntegerLessEqual(left, right)
        | IntegerGreaterThan(left, right)
        | IntegerGreaterEqual(left, right)
        | Add(left, right)
        | Sub(left, right)
        | Mul(left, right)
        | Div(left, right)
        | Rem(left, right)
        | Pow(left, right)
        | LessThan(left, right)
        | Equal(left, right)
        | NotEqual(left, right)
        | LessEqual(left, right)
        | GreaterThan(left, right)
        | GreaterEqual(left, right) => {
            visit(left, false);
            visit(right, false);
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::super::hir::{NumericBlock, NumericFrameSlot, NumericFrameState};
    use super::*;

    fn fixture() -> NumericFunction {
        let nodes = vec![
            NumericNode::Parameter {
                register: 0,
                value_type: NumericType::Tagged,
            },
            NumericNode::TaggedToInt32(NumericValue(0)),
            NumericNode::IntegerAddImmediate(NumericValue(1), 1),
        ];
        NumericFunction {
            property_sites: Default::default(),
            constructor_field_sites: Default::default(),
            function_id: 1,
            parameter_count: 1,
            register_count: 2,
            arithmetic_op_count: 1,
            blocks: vec![NumericBlock {
                logical_pc: 0,
                osr_entry_allowed: false,
                predecessors: vec![],
                successors: vec![],
                parameters: vec![],
                parameter_registers: vec![],
                successor_arguments: vec![],
                nodes: (0..nodes.len()).map(NumericValue).collect(),
                terminator: NumericTerminator::Return(NumericValue(2)),
            }],
            nodes,
            frame_states: [1, 2]
                .into_iter()
                .map(|index| NumericFrameState {
                    point: NumericFramePoint::Node(NumericValue(index)),
                    frames: Box::new([otter_vm::deopt::DeoptFrame {
                        function_id: 1,
                        byte_pc: 8,
                        entry: None,
                        slots: (vec![
                            NumericFrameSlot::Value(NumericValue(0)),
                            NumericFrameSlot::Undefined,
                        ])
                        .into(),
                    }]),
                    virtual_objects: Box::default(),
                })
                .collect(),
            direct_call_targets: vec![],
            operand_values: vec![],
        }
    }

    #[test]
    fn boxed_arithmetic_removes_only_obsolete_overflow_state() {
        let mut hir = fixture();
        let view = JitCompileSnapshot::without_feedback(1, 1, 2, vec![]);
        assert_eq!(relax(&mut hir, &view), 1);
        assert_eq!(hir.frame_states.len(), 1);
        assert_eq!(
            hir.frame_states[0].point,
            NumericFramePoint::Node(NumericValue(1))
        );
        assert_eq!(hir.nodes[1], NumericNode::TaggedToNumber(NumericValue(0)));
        assert_eq!(
            hir.blocks[0].nodes,
            [
                NumericValue(0),
                NumericValue(1),
                NumericValue(3),
                NumericValue(2)
            ]
        );
        super::super::select(&hir).unwrap();
    }

    #[test]
    fn boxed_arithmetic_preserves_integer_and_phi_demands() {
        let view = JitCompileSnapshot::without_feedback(1, 1, 2, vec![]);
        for (operand, edge) in [(2, false), (1, false), (2, true)] {
            let mut hir = fixture();
            if edge {
                hir.blocks[0]
                    .successor_arguments
                    .push(vec![NumericValue(operand)]);
            } else {
                hir.nodes
                    .push(NumericNode::IntegerAndImmediate(NumericValue(operand), 7));
                hir.blocks[0].nodes.push(NumericValue(3));
            }
            let original = hir.clone();
            assert_eq!(relax(&mut hir, &view), 0);
            assert_eq!(hir, original);
        }
    }
}
