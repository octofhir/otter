//! Explicit named-property probe, committed cold call and completion CFG.
//!
//! # Contents
//! - Machine values and blocks for a source-owned property operation.
//! - Collector-visible cold operands and explicit native status control.
//!
//! # Invariants
//! - The probe never allocates or reenters JavaScript.
//! - Only the cold call owns a safepoint; success joins through ordinary SSA.
//! - Throw and Fatal are explicit terminators, never emitter-hidden branches.
//! - The untraced cell address refers to code-owned stable memory.
//!
//! # See also
//! - `super::SelectionCfg` assigns block identities before allocation.
//! - `super::super::MachineCacheIrSite` owns source identity and CacheIR.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct Blocks {
    pub hit: MachineBlock,
    pub cold: MachineBlock,
    pub success: MachineBlock,
    pub throw: Option<MachineBlock>,
    pub fatal: MachineBlock,
    pub join: MachineBlock,
}

#[derive(Clone, Copy)]
pub(super) struct Values {
    pub hit: MachineValue,
    pub payload: MachineValue,
    pub cell: MachineValue,
    pub cold_payload: MachineValue,
    pub status: MachineValue,
}

impl Values {
    pub fn new(representations: &mut Vec<MachineRepresentation>) -> Self {
        Self {
            hit: push_value(representations, MachineRepresentation::Boolean),
            payload: push_value(representations, MachineRepresentation::Tagged),
            cell: push_value(representations, MachineRepresentation::Int64),
            cold_payload: push_value(representations, MachineRepresentation::Tagged),
            status: push_value(representations, MachineRepresentation::NativeStatus),
        }
    }
}

pub(super) fn site(hir: &NumericFunction, block: usize) -> Option<hir::NumericValue> {
    let value = *hir.blocks.get(block)?.nodes.last()?;
    matches!(
        hir.nodes[value.0],
        NumericNode::PropertyLoad { .. } | NumericNode::PropertyStore { .. }
    )
    .then_some(value)
}

pub(super) fn exceptional(hir: &NumericFunction, block: usize) -> Option<usize> {
    match hir.nodes[site(hir, block)?.0] {
        NumericNode::PropertyLoad {
            exceptional_edge, ..
        } => exceptional_edge.map(usize::from),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_block(
    target_spec: &TargetSpec,
    selected: SelectedBlock,
    hir: &NumericFunction,
    cfg: &SelectionCfg,
    block_index: usize,
    values: Values,
    inputs: (MachineValue, Option<MachineValue>),
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    call_descriptors: &mut Vec<CallDescriptor>,
    next_safepoint: &mut u32,
    instructions: &mut Vec<MachineInstruction>,
) -> Result<MachineBlockData, super::super::VerificationError> {
    let (receiver, stored) = inputs;
    let first = MachineInstructionId(instructions.len() as u32);
    let node = site(hir, block_index).ok_or(super::super::VerificationError::InvalidBlock(
        cfg.originals[block_index],
    ))?;
    let blocks = cfg.properties[&block_index];
    let mut result = MachineBlockData {
        first,
        end: first,
        predecessors: vec![],
        successors: vec![],
        parameters: vec![],
        successor_arguments: vec![],
    };
    match selected {
        SelectedBlock::PropertyCold(_) => {
            let source = &hir.property_sites[&node];
            let mut descriptor = binding_value_descriptor(
                target_spec,
                source.logical_pc,
                source.byte_pc,
                if stored.is_some() { 3 } else { 2 },
            );
            if let CallTarget::CommittedRuntime { target, .. } = &mut descriptor.target {
                *target = if stored.is_some() {
                    otter_vm::native_abi::STUB_JIT_STORE_PROPERTY
                } else {
                    otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY
                };
            }
            descriptor.arguments = vec![MachineRepresentation::Tagged];
            if stored.is_some() {
                descriptor.arguments.push(MachineRepresentation::Tagged);
            }
            descriptor.arguments.push(MachineRepresentation::Int64);
            let descriptor_index = intern_call_descriptor(call_descriptors, descriptor);
            let mut operands = vec![MachineOperand::location_input(receiver)];
            if let Some(value) = stored {
                operands.push(MachineOperand::location_input(value));
            }
            operands.extend([
                MachineOperand::location_input(values.cell),
                MachineOperand::register_output(values.cold_payload),
                MachineOperand::register_output(values.status),
            ]);
            let mut call =
                MachineInstruction::plain(MachineOpcode::Call(descriptor_index as u32), operands);
            call.clobbers = call_descriptors[descriptor_index].clobbers.clone();
            call.safepoint = Some(SafepointId(*next_safepoint));
            *next_safepoint += 1;
            let state_index = hir
                .frame_states
                .iter()
                .position(|state| state.point == NumericFramePoint::Node(node))
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            inline_reentry::select_frames(
                hir,
                state_index,
                machine_values,
                representations,
                instructions,
                &mut call,
            )?;
            attach_frame_state_tagged_roots(hir, machine_values, state_index, &mut call);
            instructions.push(call);
            let mut branch = MachineInstruction::plain(
                MachineOpcode::BranchNativeStatus,
                vec![MachineOperand::register_input(values.status)],
            );
            branch.clobbers = target_spec
                .clobbers(TargetClobberSet::StatusScratch)
                .to_vec();
            branch.control = ControlFlow::Branch;
            instructions.push(branch);
            result.predecessors = vec![cfg.originals[block_index]];
            let throw = exceptional(hir, block_index)
                .map(|edge| cfg.split_edges[&(block_index, edge)])
                .or(blocks.throw)
                .ok_or(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ))?;
            result.successors = vec![blocks.success, throw, blocks.fatal];
            result.successor_arguments = vec![vec![], vec![], vec![]];
        }
        SelectedBlock::PropertyHit(_) => {
            jump(instructions);
            result.predecessors = vec![cfg.originals[block_index]];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if stored.is_some() {
                vec![]
            } else {
                vec![values.payload]
            }];
        }
        SelectedBlock::PropertySuccess(_) => {
            jump(instructions);
            result.predecessors = vec![blocks.cold];
            result.successors = vec![blocks.join];
            result.successor_arguments = vec![if stored.is_some() {
                vec![]
            } else {
                vec![values.cold_payload]
            }];
        }
        SelectedBlock::PropertyThrow(_) | SelectedBlock::PropertyFatal(_) => {
            let throwing = matches!(selected, SelectedBlock::PropertyThrow(_));
            let mut terminator = MachineInstruction::plain(
                if throwing {
                    MachineOpcode::Throw
                } else {
                    MachineOpcode::Fatal
                },
                if throwing {
                    vec![MachineOperand::register_input(values.cold_payload)]
                } else {
                    vec![]
                },
            );
            terminator.control = ControlFlow::Return;
            instructions.push(terminator);
            result.predecessors = vec![blocks.cold];
        }
        SelectedBlock::PropertyJoin(_) => {
            if hir.blocks[block_index].terminator != NumericTerminator::Jump {
                return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                    first,
                ));
            }
            jump(instructions);
            result.predecessors = vec![blocks.hit, blocks.success];
            if stored.is_none() {
                result.parameters = vec![machine_value(machine_values, node)];
            }
            for edge in 0..hir.blocks[block_index].successors.len() {
                if exceptional(hir, block_index) == Some(edge) {
                    continue;
                }
                result
                    .successors
                    .push(cfg.edge_target(hir, block_index, edge));
                result.successor_arguments.push(cfg.edge_arguments(
                    hir,
                    block_index,
                    edge,
                    machine_values,
                ));
            }
        }
        _ => {
            return Err(super::super::VerificationError::OpcodeSignatureMismatch(
                first,
            ));
        }
    }
    result.end = MachineInstructionId(instructions.len() as u32);
    Ok(result)
}

fn jump(instructions: &mut Vec<MachineInstruction>) {
    let mut jump = MachineInstruction::plain(MachineOpcode::Jump, vec![]);
    jump.control = ControlFlow::Branch;
    instructions.push(jump);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::effects::{MachineAliasClass, MachineCommoning};

    #[test]
    fn intrinsic_property_load_keeps_receiver_proof_and_committed_cold_edge() {
        use otter_vm::jit::{JitCacheIrOp, JitCacheIrProgram, JitIntrinsicPrototype};
        let proof = JitIntrinsicPrototype {
            type_tag: otter_vm::collections::MAP_BODY_TYPE_TAG,
            guard: Some(otter_vm::JitBodyGuard::clear(
                // The repr(C) collection body's first field is the latch.
                otter_gc::header::HEADER_SIZE as u32,
                otter_vm::JitGuardWidth::Word32,
            )),
            proto_offset: 0x1000,
            active_realm: None,
        };
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let mut hir = super::super::tests::property_selection_hir();
            let site = hir.property_sites.get_mut(&hir::NumericValue(2)).unwrap();
            site.program = Box::new([JitCacheIrProgram {
                ops: Box::new([
                    JitCacheIrOp::LoadIntrinsicPrototype {
                        object: 0,
                        result: 1,
                        target: proof,
                    },
                    JitCacheIrOp::GuardShape {
                        object: 1,
                        shape: 0x2000,
                    },
                    JitCacheIrOp::GuardAtomSlot {
                        object: 1,
                        atom: 17,
                        value_byte: 8,
                        writable: false,
                    },
                    JitCacheIrOp::LoadField {
                        object: 1,
                        value_byte: 8,
                    },
                ]),
            }]);
            let sequence = select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                .expect("intrinsic property selection");
            let probe = sequence
                .instructions()
                .iter()
                .find(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::CacheIrLoadIntrinsicPrototype { .. }
                    )
                })
                .expect("explicit receiver proof");
            assert_eq!(probe.operands.len(), 4);
            assert!(probe.safepoint.is_none() && probe.exits.is_empty());
            let effects = probe.opcode.effects();
            assert!(effects.writes.is_empty());
            assert!(
                !effects.allocates && !effects.reentrant && !effects.throws && !effects.safepoint
            );
            assert_eq!(effects.commoning, MachineCommoning::Guard);
            assert!(effects.reads.contains(MachineAliasClass::PropertyMetadata));
            assert_eq!(
                sequence
                    .call_descriptors()
                    .iter()
                    .filter(|descriptor| {
                        matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. }
                    if target.id == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY.id)
                    })
                    .count(),
                1
            );
            sequence
                .allocate(&target)
                .expect("intrinsic property allocation");

            for invalid in [
                JitIntrinsicPrototype {
                    guard: None,
                    ..proof
                },
                JitIntrinsicPrototype {
                    proto_offset: 0,
                    ..proof
                },
                JitIntrinsicPrototype {
                    type_tag: otter_vm::string::JS_STRING_BODY_TYPE_TAG,
                    ..proof
                },
            ] {
                let site = hir.property_sites.get_mut(&hir::NumericValue(2)).unwrap();
                site.program[0].ops[0] = JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target: invalid,
                };
                assert!(
                    select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn megamorphic_load_keeps_one_pure_probe_and_the_existing_cold_call() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let mut hir = super::super::tests::property_selection_hir();
            let site = hir.property_sites.get_mut(&hir::NumericValue(2)).unwrap();
            site.program = Box::default();
            site.megamorphic_atom = Some(17);
            let sequence = select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                .expect("megamorphic property selection");
            let probes = sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::PropertyMegamorphicLoad { .. }
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(probes.len(), 1);
            let probe = probes[0];
            assert!(matches!(
                probe.opcode,
                MachineOpcode::PropertyMegamorphicLoad {
                    byte_pc: 24,
                    atom: 17
                }
            ));
            assert_eq!(probe.operands.len(), 3);
            assert_eq!(
                probe.clobbers,
                target.clobbers(TargetClobberSet::PropertyLoad)
            );
            assert!(probe.safepoint.is_none() && probe.exits.is_empty());
            let effects = probe.opcode.effects();
            assert!(effects.writes.is_empty());
            assert!(
                !effects.allocates && !effects.reentrant && !effects.throws && !effects.safepoint
            );
            assert_eq!(
                sequence
                    .call_descriptors()
                    .iter()
                    .filter(|descriptor| {
                        matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. }
                    if target.id == otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY.id)
                    })
                    .count(),
                1
            );
            sequence
                .allocate(&target)
                .expect("megamorphic register allocation");
        }
    }

    #[test]
    fn polymorphic_own_data_load_decodes_once_and_dispatches_every_shape() {
        use otter_vm::{JitCacheIrOp, JitCacheIrProgram};
        let own = |shape, value_byte, ordinary: bool| {
            let mut ops = vec![JitCacheIrOp::GuardShape { object: 0, shape }];
            if ordinary {
                ops.push(JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 5,
                    value_byte,
                    writable: false,
                });
            }
            ops.push(JitCacheIrOp::LoadField {
                object: 0,
                value_byte,
            });
            JitCacheIrProgram {
                ops: ops.into_boxed_slice(),
            }
        };
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let mut hir = super::super::tests::property_selection_hir();
            let site = hir.property_sites.get_mut(&hir::NumericValue(2)).unwrap();
            site.program =
                vec![own(7, 8, false), own(9, 16, true), own(11, 24, false)].into_boxed_slice();
            let sequence = select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                .expect("polymorphic property selection");
            let loads = sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::PropertyPolymorphicLoad { byte_pc: 24, .. }
                    )
                })
                .collect::<Vec<_>>();
            let [load] = loads.as_slice() else {
                panic!("one dispatch per site: {loads:?}");
            };
            let MachineOpcode::PropertyPolymorphicLoad { cases, .. } = &load.opcode else {
                unreachable!();
            };
            assert_eq!(
                cases
                    .iter()
                    .map(|case| (case.shape, case.value_byte, case.ordinary))
                    .collect::<Vec<_>>(),
                [(7, 8, false), (9, 16, true), (11, 24, false)]
            );
            assert_eq!(
                load.clobbers,
                target.clobbers(TargetClobberSet::PropertyLoad)
            );
            assert!(load.safepoint.is_none() && load.exits.is_empty());
            let effects = load.opcode.effects();
            assert!(effects.writes.is_empty());
            assert!(!effects.allocates && !effects.reentrant && !effects.safepoint);
            assert!(!sequence.instructions().iter().any(|instruction| matches!(
                instruction.opcode,
                MachineOpcode::CacheIrGuardShape { byte_pc: 24, .. }
                    | MachineOpcode::CacheIrLoadField { byte_pc: 24, .. }
            )));
            sequence
                .allocate(&target)
                .expect("polymorphic register allocation");
        }
    }

    #[test]
    fn store_dispatch_owns_existing_and_transition_programs_of_one_site() {
        use otter_vm::{JitCacheIrOp, JitCacheIrProgram};
        let existing = JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 7,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom: 5,
                    value_byte: 8,
                    writable: true,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte: 8,
                },
            ]
            .into_boxed_slice(),
        };
        let transition = JitCacheIrProgram {
            ops: vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: 9,
                },
                JitCacheIrOp::LoadPrototype {
                    object: 0,
                    result: 1,
                },
                JitCacheIrOp::GuardShape {
                    object: 1,
                    shape: 13,
                },
                JitCacheIrOp::GuardPrototypeNull { object: 1 },
                JitCacheIrOp::GuardExtensible {
                    object: 0,
                    value_byte: 16,
                },
                JitCacheIrOp::StoreField {
                    object: 0,
                    value_byte: 16,
                },
                JitCacheIrOp::PublishShape {
                    object: 0,
                    shape: 21,
                },
            ]
            .into_boxed_slice(),
        };
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let mut hir = super::super::tests::property_selection_hir();
            let site = hir.property_sites.get_mut(&hir::NumericValue(3)).unwrap();
            site.program = vec![existing.clone(), transition.clone()].into_boxed_slice();
            let sequence = select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                .expect("store dispatch selection");
            let dispatches = sequence
                .instructions()
                .iter()
                .filter_map(|instruction| match &instruction.opcode {
                    MachineOpcode::PropertyStoreDispatch { byte_pc: 40, cases } => {
                        Some((instruction, cases))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let [(dispatch, cases)] = dispatches.as_slice() else {
                panic!("one dispatch per store site: {dispatches:?}");
            };
            assert_eq!(cases.len(), 2);
            assert!(cases[0].transition.is_none() && cases[0].value_byte == 8);
            let added = cases[1].transition.as_ref().expect("transition case");
            assert_eq!(
                (
                    cases[1].shape,
                    added.prototype_shapes.as_ref(),
                    added.child_shape
                ),
                (9, &[13][..], 21)
            );
            assert_eq!(
                dispatch.clobbers,
                target.clobbers(TargetClobberSet::PropertyStore)
            );
            assert!(!sequence.instructions().iter().any(|instruction| matches!(
                instruction.opcode,
                MachineOpcode::CacheIrStoreField { byte_pc: 40, .. }
                    | MachineOpcode::CacheIrPublishShape { byte_pc: 40, .. }
            )));
            assert_eq!(
                sequence
                    .instructions()
                    .iter()
                    .filter(|instruction| matches!(
                        instruction.opcode,
                        MachineOpcode::CacheIrWriteBarrier { byte_pc: 40, .. }
                    ))
                    .count(),
                2
            );
            sequence
                .allocate(&target)
                .expect("store dispatch register allocation");
        }
    }

    #[test]
    fn megamorphic_store_commits_once_before_the_existing_cold_call() {
        for target in [TargetSpec::aarch64(), TargetSpec::x86_64()] {
            let mut hir = super::super::tests::property_selection_hir();
            let site = hir.property_sites.get_mut(&hir::NumericValue(3)).unwrap();
            site.program = Box::default();
            site.megamorphic_atom = Some(17);
            let sequence = select_with_loop_entries(&target, &hir, &hir.plan_loop_entries(), None)
                .expect("megamorphic property selection");
            let stores = sequence
                .instructions()
                .iter()
                .filter(|instruction| {
                    matches!(
                        instruction.opcode,
                        MachineOpcode::PropertyMegamorphicStore { .. }
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(stores.len(), 1);
            let store = stores[0];
            assert!(matches!(
                store.opcode,
                MachineOpcode::PropertyMegamorphicStore {
                    byte_pc: 40,
                    atom: 17
                }
            ));
            assert_eq!(store.operands.len(), 4);
            assert_eq!(
                store.clobbers,
                target.clobbers(TargetClobberSet::PropertyStore)
            );
            assert!(store.safepoint.is_none() && store.exits.is_empty());
            let effects = store.opcode.effects();
            assert!(effects.reads.contains(MachineAliasClass::Shape));
            assert!(effects.reads.contains(MachineAliasClass::PropertyMetadata));
            assert!(effects.reads.contains(MachineAliasClass::PropertyField));
            assert!(effects.writes.contains(MachineAliasClass::PropertyField));
            assert_eq!(effects.commoning, MachineCommoning::Never);
            assert!(
                !effects.allocates && !effects.reentrant && !effects.throws && !effects.safepoint
            );
            assert_eq!(
                sequence
                    .instructions()
                    .iter()
                    .filter(|instruction| {
                        matches!(
                            instruction.opcode,
                            MachineOpcode::CacheIrWriteBarrier { .. }
                        )
                    })
                    .count(),
                1
            );
            assert_eq!(
                sequence
                    .call_descriptors()
                    .iter()
                    .filter(|descriptor| {
                        matches!(descriptor.target, CallTarget::CommittedRuntime { target, .. }
                    if target.id == otter_vm::native_abi::STUB_JIT_STORE_PROPERTY.id)
                    })
                    .count(),
                1
            );
            sequence
                .allocate(&target)
                .expect("megamorphic register allocation");
        }
    }
}
