//! Final observation records for the numeric words generated stores publish.
//!
//! # Contents
//! - [`finalize`] refreshes existing store and literal records after Phi and
//!   conversion rewriting have chosen their actual stored representations.
//!
//! # Invariants
//! - Float64 boxing produces an encoded Double even for integral values.
//! - Int32 narrowing knowledge never changes an existing Tagged word's encoding.
//! - Only a producer's physical numeric representation replaces its old record;
//!   object and nullish bounds retain the builder's original path proof.
//! - The source snapshot remains the sole defining-field authority.
//!
//! # See also
//! - [`super::builder_field_type`] selects path-dependent nonnumeric bounds.
//! - [`crate::field_store`] owns the selected observation record.

use std::sync::Arc;

use otter_vm::{JitCacheIrOp, JitCompileSnapshot};
use smallvec::SmallVec;

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};
use crate::field_store::{FieldStoreOwner, StoreValueBound};

/// An exact bound on the actual stored Number word, without path knowledge.
fn numeric_word(graph: &Graph, value: NodeId) -> Option<StoreValueBound> {
    let data = graph.node(value);
    let integer = match data.kind {
        Kind::ConstTagged(bits) => {
            let value = otter_vm::Value::from_bits(bits);
            if !value.is_number() {
                return None;
            }
            value.as_i32().map(Some)
        }
        Kind::Int32ToTagged => Some(match graph.node(data.inputs[0]).kind {
            Kind::ConstInt32(value) => Some(value),
            _ => None,
        }),
        Kind::Float64ToTagged => None,
        _ if data.repr == Repr::Float64 => None,
        _ => return None,
    };
    Some(StoreValueBound {
        shapes: Default::default(),
        nullish: 0,
        int32: integer.is_some(),
        double: integer.is_none(),
        int32_constant: integer.flatten(),
    })
}

pub(crate) fn finalize(
    graph: &mut Graph,
    layout: &[BlockId],
    view: &JitCompileSnapshot,
    inline_views: &[Arc<JitCompileSnapshot>],
) {
    for &block in layout {
        for node in graph.block(block).body.clone() {
            let source = super::metadata::source_view(view, inline_views, graph, node);
            match graph.node(node).kind.clone() {
                Kind::StoreOwnField { field, .. } | Kind::StoreDoubleOwnField { field, .. } => {
                    let Some(bound) = numeric_word(graph, graph.node(node).inputs[1]) else {
                        continue;
                    };
                    let byte_pc = source
                        .instructions
                        .get(graph.node(node).pc as usize)
                        .map(|instruction| instruction.byte_pc);
                    let mut receivers = SmallVec::<[u32; 4]>::new();
                    if let Some(programs) = byte_pc.and_then(|pc| source.property_programs.get(&pc))
                    {
                        for program in programs {
                            let Some(object) = program.ops.iter().find_map(|op| match *op {
                                JitCacheIrOp::StoreField {
                                    object,
                                    field: stored,
                                } if stored == field => Some(object),
                                _ => None,
                            }) else {
                                continue;
                            };
                            if let Some(shape) = program.ops.iter().find_map(|op| match *op {
                                JitCacheIrOp::GuardShape {
                                    object: guarded,
                                    shape,
                                } if object == guarded => Some(shape),
                                _ => None,
                            }) && !receivers.contains(&shape)
                            {
                                receivers.push(shape);
                            }
                        }
                    }
                    let existing = match &graph.node(node).kind {
                        Kind::StoreOwnField { owners, .. }
                        | Kind::StoreDoubleOwnField { owners, .. } => owners,
                        _ => unreachable!(),
                    };
                    for &(shape, _) in existing {
                        if !receivers.contains(&shape) {
                            receivers.push(shape);
                        }
                    }
                    let owners: SmallVec<[(u32, FieldStoreOwner); 4]> = receivers
                        .into_iter()
                        .filter_map(|shape| {
                            let record = source.field_types.get(&(shape, field))?;
                            Some((shape, FieldStoreOwner::select(record, Some(&bound))))
                        })
                        .collect();
                    if !owners.is_empty() {
                        let owners = if owners.iter().any(|(_, owner)| owner.observes()) {
                            owners
                        } else {
                            SmallVec::new()
                        };
                        match &mut graph.node_mut(node).kind {
                            Kind::StoreOwnField { owners: target, .. }
                            | Kind::StoreDoubleOwnField { owners: target, .. } => *target = owners,
                            _ => unreachable!(),
                        }
                    }
                }
                Kind::StoreNamedProperty {
                    byte_pc,
                    receiver_shape,
                    proved,
                    ..
                } => {
                    let Some(bound) = numeric_word(graph, graph.node(node).inputs[1]) else {
                        continue;
                    };
                    let Some(programs) = super::feedback::store_node_programs(
                        source,
                        byte_pc,
                        receiver_shape,
                        proved,
                    ) else {
                        continue;
                    };
                    let owners: Option<SmallVec<[FieldStoreOwner; 4]>> = programs
                        .iter()
                        .map(|program| {
                            let (object, field) = program.ops.iter().find_map(|op| match *op {
                                JitCacheIrOp::StoreField { object, field } => Some((object, field)),
                                _ => None,
                            })?;
                            let target = program.ops.iter().rev().find_map(|op| match *op {
                                JitCacheIrOp::PublishShape {
                                    object: guarded,
                                    shape,
                                }
                                | JitCacheIrOp::GuardShape {
                                    object: guarded,
                                    shape,
                                } if object == guarded => Some(shape),
                                _ => None,
                            })?;
                            Some(FieldStoreOwner::select(
                                source.field_types.get(&(target, field))?,
                                Some(&bound),
                            ))
                        })
                        .collect();
                    if let Some(owners) = owners
                        && let Kind::StoreNamedProperty { owners: target, .. } =
                            &mut graph.node_mut(node).kind
                    {
                        *target = owners;
                    }
                }
                Kind::NewObjectLiteral {
                    fields: Some(mut fields),
                } => {
                    let Some(plan) = source.literal_allocations.objects.get(&graph.node(node).pc)
                    else {
                        continue;
                    };
                    for (index, &value) in graph.node(node).inputs.iter().enumerate() {
                        if let Some(bound) = numeric_word(graph, value)
                            && let Some(record) = source.field_types.get(&(
                                plan.shape,
                                otter_vm::object::FieldLocation::inline(index as u32),
                            ))
                            && let Some(target) = fields.get_mut(index)
                        {
                            *target = FieldStoreOwner::select(record, Some(&bound));
                        }
                    }
                    if let Kind::NewObjectLiteral { fields: target } =
                        &mut graph.node_mut(node).kind
                    {
                        *target = Some(fields);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::jit::{JitCacheIrProgram, JitFieldType, JitFieldTypeGuard};
    use otter_vm::object::{FieldLocation, field_type};

    #[test]
    fn numeric_store_selection_uses_final_double_box_instead_of_int32_knowledge() {
        let field = FieldLocation::inline(0);
        let mut view = JitCompileSnapshot::without_feedback(
            1,
            0,
            3,
            vec![otter_vm::jit::JitTestInstruction::new(
                otter_bytecode::Op::StoreProperty,
                0,
                0,
                vec![
                    otter_bytecode::Operand::Register(0),
                    otter_bytecode::Operand::ConstIndex(0),
                    otter_bytecode::Operand::Register(1),
                    otter_bytecode::Operand::Register(2),
                ],
            )],
        );
        view.property_programs.insert(
            0,
            vec![JitCacheIrProgram {
                ops: vec![
                    JitCacheIrOp::GuardShape {
                        object: 0,
                        shape: 128,
                    },
                    JitCacheIrOp::StoreField { object: 0, field },
                ]
                .into_boxed_slice(),
            }],
        );
        let record = JitFieldType {
            guard: JitFieldTypeGuard {
                owner_shape: 136,
                expected: field_type::INT32,
            },
            shapes: Default::default(),
            stored_shapes: Default::default(),
            nullish_mask: 0,
        };
        view.field_types.insert((128, field), record.clone());
        let mut graph = Graph::default();
        let block = graph.new_block();
        let receiver = graph.constant(Kind::ConstTagged(otter_vm::Value::undefined().to_bits()));
        let number = graph.constant(Kind::ConstFloat64(0.0f64.to_bits()));
        let boxed = graph.add_node(Kind::Float64ToTagged, &[number], Repr::Tagged);
        graph.node_mut(boxed).block = Some(block);
        graph.block_mut(block).body.push(boxed);
        let old = FieldStoreOwner::select(
            &record,
            Some(&StoreValueBound {
                shapes: Default::default(),
                nullish: 0,
                int32: true,
                double: false,
                int32_constant: None,
            }),
        );
        assert!(!old.observes());
        let node = graph.add_node(
            Kind::StoreNamedProperty {
                plan: super::super::ir::NamedStorePlan::Programs,
                byte_pc: 0,
                owners: smallvec::smallvec![old],
                receiver_shape: Some(128),
                fresh: false,
                proved: false,
            },
            &[receiver, boxed],
            Repr::None,
        );
        graph.node_mut(node).block = Some(block);
        graph.block_mut(block).body.push(node);
        let own = graph.add_node(
            Kind::StoreOwnField {
                field,
                atom: 1,
                owners: Default::default(),
            },
            &[receiver, boxed],
            Repr::None,
        );
        graph.node_mut(own).block = Some(block);
        graph.block_mut(block).body.push(own);
        view.literal_allocations.objects.insert(
            0,
            otter_vm::jit::JitObjectLiteralAllocationPlan {
                shape: 128,
                shape_byte: 0,
                cell_bytes: 64,
                header_word: 0,
                inline_capacity: 1,
                value_count: 1,
                fields: otter_vm::object::FieldLayout::current(),
            },
        );
        let literal = graph.add_node(
            Kind::NewObjectLiteral {
                fields: Some(smallvec::smallvec![FieldStoreOwner::select(
                    &record,
                    Some(&StoreValueBound {
                        shapes: Default::default(),
                        nullish: 0,
                        int32: true,
                        double: false,
                        int32_constant: None
                    })
                )]),
            },
            &[boxed],
            Repr::Tagged,
        );
        graph.node_mut(literal).block = Some(block);
        graph.block_mut(block).body.push(literal);
        finalize(&mut graph, &[block], &view, &[]);
        let Kind::StoreNamedProperty { owners, .. } = &graph.node(node).kind else {
            unreachable!()
        };
        assert_eq!(
            owners[0].owner_shape, 136,
            "integral Double widens a raw-Int32 summary"
        );
        let Kind::StoreOwnField { owners, .. } = &graph.node(own).kind else {
            unreachable!()
        };
        assert_eq!(
            owners[0].1.owner_shape, 136,
            "recover a formerly empty own-store record from its source program"
        );
        let Kind::NewObjectLiteral {
            fields: Some(fields),
        } = &graph.node(literal).kind
        else {
            unreachable!()
        };
        assert_eq!(
            fields[0].owner_shape, 136,
            "literal fits observe the same actual Double word"
        );
    }
}
