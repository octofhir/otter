//! Speculative monomorphic own-data property loads.
//!
//! # Contents
//! - [`speculated_load`] — the one admission rule deciding whether a
//!   `LoadProperty` site becomes a straight-line shape-proven load instead of a
//!   probe plus committed cold call.
//! - [`select`] — its Machine form: a receiver shape proof, one exact
//!   pre-operation `GuardCondition` exit, and an unchecked slot read.
//! - [`own_data_slot`] — the CacheIR program pattern of one hidden class's own
//!   data slot, shared with the non-deopting polymorphic shape dispatch.
//!
//! # Invariants
//! - Only a site whose single baked CacheIR program is an own data slot of one
//!   hidden class, outside any local catch, never megamorphic, and never an
//!   exotic `.length` read qualifies.
//! - A site whose earlier optimized generation exited with `ShapeGuard` keeps
//!   the committed probe/cold-call form, so a recompile cannot repeat the
//!   failed speculation.
//! - The exit precedes every effect of the load: the interpreter resumes at the
//!   `LoadProperty` itself and performs the access exactly once.
//! - The proof has no incoming condition and no frame state, so GVN commons an
//!   identical dominating proof and LICM may hoist it out of a loop whose body
//!   writes no shape, descriptor or prototype state. The exit and the slot read
//!   stay at the site.
//!
//! # See also
//! - `super::property_cfg` owns the committed probe/cold-call form.
//! - `super::constructor_effects` is the analogous speculative store.

use super::*;
use otter_bytecode::Op;

/// One admitted shape-proven own-data load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ShapeLoad {
    /// Hidden class every receiver observed at the site had.
    pub shape: u32,
    /// Byte offset of the data slot inside the receiver's value slab.
    pub value_byte: u32,
    /// Whether the program also required ordinary named-lookup state.
    pub ordinary: bool,
}

/// Admit `LoadProperty` at `logical_pc` of `view` as a speculative load.
pub(super) fn speculated_load(view: &JitCompileSnapshot, logical_pc: u32) -> Option<ShapeLoad> {
    let instruction = view.instructions.get(logical_pc as usize)?;
    let code = &view.code_block;
    if instruction.op(code) != Op::LoadProperty
        || instruction.load_array_length
        || view.cage_base == 0
        || code
            .control_flow()
            .enclosing_exception_region(logical_pc)
            .is_some_and(|region| region.catch_pc.is_some())
        || (view.property_lookup_cache.is_some()
            && view
                .property_megamorphic_accesses
                .contains_key(&instruction.byte_pc))
        || view
            .optimized_exit_reasons
            .get(&logical_pc)
            .is_some_and(|reasons| reasons.contains(&ExitReason::ShapeGuard))
    {
        return None;
    }
    let [program] = view.property_programs.get(&instruction.byte_pc)?.as_slice() else {
        return None;
    };
    own_data_slot(program)
}

/// Recognize a program that reads one own data slot of one hidden class:
/// `GuardShape`, an optional read-only `GuardAtomSlot` naming the same slot,
/// and `LoadField`, all on the receiver.
pub(super) fn own_data_slot(program: &otter_vm::JitCacheIrProgram) -> Option<ShapeLoad> {
    let (shape, value_byte, ordinary) = match *program.ops {
        [
            otter_vm::JitCacheIrOp::GuardShape { object: 0, shape },
            otter_vm::JitCacheIrOp::LoadField {
                object: 0,
                value_byte,
            },
        ] => (shape, value_byte, false),
        [
            otter_vm::JitCacheIrOp::GuardShape { object: 0, shape },
            otter_vm::JitCacheIrOp::GuardAtomSlot {
                object: 0,
                value_byte: slot,
                writable: false,
                ..
            },
            otter_vm::JitCacheIrOp::LoadField {
                object: 0,
                value_byte,
            },
        ] if slot == value_byte => (shape, value_byte, true),
        _ => return None,
    };
    (shape != 0 && value_byte % 8 == 0).then_some(ShapeLoad {
        shape,
        value_byte,
        ordinary,
    })
}

/// Recognize a store program the store dispatch can own: an existing
/// writable own slot, or an add transition whose missing-key proof is a null
/// prototype or a guarded prototype chain ending in null.
pub(super) fn store_case(
    program: &otter_vm::JitCacheIrProgram,
) -> Option<super::super::PropertyStoreCase> {
    use otter_vm::JitCacheIrOp as Op;
    match *program.ops {
        [
            Op::GuardShape { object: 0, shape },
            Op::GuardAtomSlot {
                object: 0,
                value_byte: slot,
                writable: true,
                ..
            },
            Op::StoreField {
                object: 0,
                value_byte,
            },
        ] if slot == value_byte && shape != 0 && value_byte % 8 == 0 => {
            Some(super::super::PropertyStoreCase {
                shape,
                value_byte,
                transition: None,
            })
        }
        [Op::GuardShape { object: 0, shape }, ref rest @ ..] if shape != 0 => {
            let (links, tail) = rest.split_last_chunk::<4>()?;
            let [
                Op::GuardPrototypeNull { object: last },
                Op::GuardExtensible {
                    object: 0,
                    value_byte: slot,
                },
                Op::StoreField {
                    object: 0,
                    value_byte,
                },
                Op::PublishShape {
                    object: 0,
                    shape: child_shape,
                    new_len,
                    initialize_inline,
                },
            ] = *tail
            else {
                return None;
            };
            if slot != value_byte || value_byte % 8 != 0 || child_shape == 0 {
                return None;
            }
            let mut prototype_shapes = Vec::with_capacity(links.len() / 2);
            let mut holder = 0u8;
            for link in links.chunks(2) {
                let [
                    Op::LoadPrototype { object, result: 1 },
                    Op::GuardShape {
                        object: 1,
                        shape: prototype,
                    },
                ] = *link
                else {
                    return None;
                };
                if object != holder || prototype == 0 {
                    return None;
                }
                prototype_shapes.push(prototype);
                holder = 1;
            }
            (last == holder).then(|| super::super::PropertyStoreCase {
                shape,
                value_byte,
                transition: Some(super::super::PropertyStoreTransition {
                    prototype_shapes: prototype_shapes.into_boxed_slice(),
                    child_shape,
                    new_len,
                    initialize_inline,
                }),
            })
        }
        _ => None,
    }
}

/// Select the proof, its exact pre-operation exit, and the slot read.
#[allow(clippy::too_many_arguments)]
pub(super) fn select(
    target_spec: &TargetSpec,
    hir: &NumericFunction,
    receiver: MachineValue,
    result: MachineValue,
    byte_pc: u32,
    load: ShapeLoad,
    state_index: usize,
    exits: Box<[MachineExit]>,
    machine_values: &[MachineValue],
    representations: &mut Vec<MachineRepresentation>,
    instructions: &mut Vec<MachineInstruction>,
) {
    let proven = push_value(representations, MachineRepresentation::Boolean);
    let mut proof = MachineInstruction::plain(
        MachineOpcode::PropertyShapeProof {
            byte_pc,
            shape: load.shape,
            ordinary: load.ordinary,
        },
        vec![
            MachineOperand::location_input(receiver),
            MachineOperand::register_output(proven),
        ],
    );
    proof.clobbers = property_load_clobbers(target_spec);
    instructions.push(proof);

    let mut require = MachineInstruction::plain(
        MachineOpcode::GuardCondition,
        vec![MachineOperand::register_input(proven)],
    );
    require.clobbers = target_spec
        .clobbers(TargetClobberSet::StatusScratch)
        .to_vec();
    attach_frame_state(hir, machine_values, state_index, exits, &mut require);
    instructions.push(require);

    let mut read = MachineInstruction::plain(
        MachineOpcode::PropertySlotLoad {
            byte_pc,
            value_byte: load.value_byte,
        },
        vec![
            MachineOperand::location_input(receiver),
            MachineOperand::register_output(result),
        ],
    );
    read.clobbers = property_load_clobbers(target_spec);
    instructions.push(read);
}
