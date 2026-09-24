//! Speculative monomorphic own-data property loads.
//!
//! # Contents
//! - [`speculated_load`] — the one admission rule deciding whether a
//!   `LoadProperty` site becomes a straight-line shape-proven load instead of a
//!   probe plus committed cold call.
//! - [`select`] — its Machine form: a receiver shape proof, one exact
//!   pre-operation `GuardCondition` exit, and an unchecked slot read.
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
