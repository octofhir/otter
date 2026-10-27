//! Canonical Graph frame geometry and deoptimization recipes for every backend.
//!
//! # Contents
//! - [`SlotLayout`] places tagged homes, exception scratch and untagged homes.
//! - [`deopt_frames`] maps complete inline states to the VM's recovery frames.
//! - [`exit_reason`] selects the shared semantic exit and recovery action.
//! - [`allocation_exit_reason`] distinguishes receiver and value probe misses.
//!
//! # Invariants
//! - Only initialized tagged words are collector roots; unboxed homes follow.
//! - Reconstruction reads canonical homes, constants, or the machine registers
//!   the shared exit handler saves in its register dump.
//! - A recipe lists only registers whose value is not the literal `undefined`;
//!   every other register resumes `undefined`.
//! - Byte offsets fit the signed VM recipe and the native frame's root indices.
//! - Backend encoders consume this geometry without owning a second frame map.
//! - Frame publication may be deferred only across emitted operations with no
//!   collecting, reentrant, throwing or loop-poll boundary. Eager exits publish
//!   the one reserved record before any runtime reconstruction.
//!
//! # See also
//! - [`super::regalloc`] for final physical homes and representation ownership.
//! - `otter_vm::deopt` for the one interpreter reconstruction contract.

use otter_vm::deopt::{
    DeoptFrame, DeoptFrameEntry, DeoptLocation, DeoptRepr, DeoptSlot, VirtualObject,
    VirtualObjectId, VirtualObjectKind,
};
use otter_vm::{
    native_abi::{self, ExitAction, ExitReason},
    value::tag::VALUE_UNDEFINED,
};

use super::ir::{DeoptReason, FrameStateId, Graph, Kind, NodeId, Repr};
use super::regalloc::{Allocation, Location};
use crate::Unsupported;

/// Whether every emitted operation can execute without observing the frame
/// chain. The window and actual-cache consumers require a complete record;
/// loop backedges own a runtime poll independently of node call properties.
pub(crate) fn can_defer_frame(
    shape: crate::call_linkage::EntryShape,
    graph: &Graph,
    allocation: &Allocation,
) -> bool {
    shape.lazy_window
        && !shape.derived
        && graph.blocks.iter().all(|block| {
            block
                .phis
                .iter()
                .chain(&block.body)
                .chain(&block.control)
                .all(|&id| {
                    if allocation.node(id).skipped {
                        return true;
                    }
                    let kind = &graph.node(id).kind;
                    let properties = kind.properties();
                    !properties.call
                        && !properties.may_collect
                        && !properties.can_throw
                        && !properties.lazy_deopt
                        && !matches!(
                            kind,
                            Kind::JumpLoop(_)
                                | Kind::Generic { .. }
                                | Kind::LoadWindow(_)
                                | Kind::CheckArgumentsElided
                                | Kind::LoadActualArgument
                                | Kind::SelectArgument { .. }
                        )
                })
        })
}

/// Classify an emitted pre-effect probe by its operation, independently of the
/// source opcode: an ordinary `New` can allocate either a receiver or an Array.
pub(crate) fn allocation_exit_reason(kind: &Kind) -> ExitReason {
    match kind {
        Kind::NewReceiver(_) => ExitReason::ReceiverAllocationMiss,
        _ => ExitReason::AllocationMiss,
    }
}

/// Frame-slot geometry shared by ordinary call and OSR entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotLayout {
    pub(crate) spill_tagged: u32,
    pub(crate) tagged: u32,
    pub(crate) untagged: u32,
}

impl SlotLayout {
    pub(crate) fn of(allocation: &Allocation) -> Result<Self, Unsupported> {
        let tagged = allocation
            .tagged_slots
            .checked_add(1)
            .ok_or(Unsupported::OperandShape(
                "graph spill region exceeds frame metadata",
            ))?;
        let layout = Self {
            spill_tagged: allocation.tagged_slots,
            tagged,
            untagged: allocation.untagged_slots,
        };
        layout.validate(0)?;
        Ok(layout)
    }

    /// Validate the whole reservation before emission or root publication.
    pub(crate) fn validate(self, register_count: u16) -> Result<(), Unsupported> {
        if self.tagged > u32::from(u16::MAX)
            || self
                .tagged
                .checked_add(self.untagged)
                .and_then(|words| words.checked_mul(8))
                .and_then(|bytes| {
                    bytes.checked_add(
                        31 + u32::from(register_count) * 8
                            + std::mem::size_of::<native_abi::Frame>() as u32,
                    )
                })
                .is_none_or(|bytes| bytes > i32::MAX as u32)
        {
            return Err(Unsupported::OperandShape(
                "graph spill region exceeds frame metadata",
            ));
        }
        Ok(())
    }

    /// Aligned canonical homes followed by the saved interpreter root words.
    pub(crate) fn bytes(self) -> u32 {
        ((self.tagged + self.untagged) * 8).next_multiple_of(16) + 16
    }

    pub(crate) fn offset(self, location: Location) -> u32 {
        match location {
            Location::TaggedSlot(index) => {
                debug_assert!(index < self.tagged);
                index * 8
            }
            Location::UntaggedSlot(index) => {
                debug_assert!(index < self.untagged);
                (self.tagged + index) * 8
            }
            _ => unreachable!("not a canonical frame slot"),
        }
    }

    pub(crate) fn exception_scratch(self) -> Location {
        Location::TaggedSlot(self.spill_tagged)
    }
}

/// Lower the complete inline chain, outermost first, with values in
/// [`Graph::state_values`] order.
pub(crate) fn deopt_frames(
    graph: &Graph,
    slots: SlotLayout,
    state: FrameStateId,
    locations: &[Location],
) -> (Box<[DeoptFrame]>, Box<[VirtualObject<DeoptSlot>]>) {
    let mut locations = locations.iter().copied();
    let mut virtual_objects: Vec<(NodeId, VirtualObject<DeoptSlot>)> = Vec::new();
    let mut slot =
        |value: NodeId| recipe_slot(graph, slots, value, &mut locations, &mut virtual_objects);
    let frames = graph
        .state_chain(state)
        .into_iter()
        .map(|state| {
            let data = graph.frame_state(state);
            let entry = data.caller.as_ref().map(|caller| DeoptFrameEntry {
                return_register: caller.return_register,
                this: slot(caller.this),
                closure: slot(caller.closure),
                new_target: slot(caller.new_target),
            });
            let arguments = data
                .caller
                .as_ref()
                .and_then(|caller| caller.arguments.as_ref())
                .map(|arguments| arguments.iter().map(|&value| slot(value)).collect());
            // Each value consumes its location in state order; a later write
            // to a register replaces an earlier one, and a literal `undefined`
            // is the window's resumption default, which the recipe omits.
            let undefined =
                DeoptSlot::physical(DeoptLocation::Literal(VALUE_UNDEFINED), DeoptRepr::Tagged);
            let mut registers = std::collections::BTreeMap::new();
            for &(register, value) in &data.registers {
                registers.insert(register, slot(value));
            }
            DeoptFrame {
                function_id: data.function_id,
                byte_pc: data.byte_pc,
                entry,
                register_count: data.register_count,
                slots: registers
                    .into_iter()
                    .filter(|(_, slot)| *slot != undefined)
                    .collect(),
                arguments,
            }
        })
        .collect();
    assert!(
        locations.next().is_none(),
        "one canonical location per reconstruction value"
    );
    let virtual_objects = virtual_objects
        .into_iter()
        .map(|(_, object)| object)
        .collect();
    (frames, virtual_objects)
}

/// The recipe slot of one state value. A virtual object's fields consume
/// their locations at its first occurrence, as
/// [`Graph::state_values`] lists them, and its recipe follows the recipes
/// of the virtual objects its fields name, so every reference points back.
fn recipe_slot(
    graph: &Graph,
    slots: SlotLayout,
    value: NodeId,
    locations: &mut impl Iterator<Item = Location>,
    virtual_objects: &mut Vec<(NodeId, VirtualObject<DeoptSlot>)>,
) -> DeoptSlot {
    if let Kind::VirtualRestArray(skip) = graph.node(value).kind {
        if let Some((_, object)) = virtual_objects.iter().find(|(node, _)| *node == value) {
            return DeoptSlot::virtual_object(object.id);
        }
        // Its elements are the activation's own actuals past `skip`.
        let id = VirtualObjectId(virtual_objects.len() as u32);
        virtual_objects.push((
            value,
            VirtualObject {
                id,
                kind: VirtualObjectKind::RestArray { skip },
                fields: Box::default(),
            },
        ));
        return DeoptSlot::virtual_object(id);
    }
    let Kind::VirtualObject { shape } = graph.node(value).kind else {
        let location = locations.next().expect("a location per state value");
        return deopt_slot(graph, slots, value, location);
    };
    if let Some((_, object)) = virtual_objects.iter().find(|(node, _)| *node == value) {
        return DeoptSlot::virtual_object(object.id);
    }
    let fields: Box<[DeoptSlot]> = graph
        .node(value)
        .inputs
        .iter()
        .map(|&field| recipe_slot(graph, slots, field, locations, virtual_objects))
        .collect();
    let id = VirtualObjectId(virtual_objects.len() as u32);
    virtual_objects.push((
        value,
        VirtualObject {
            id,
            kind: VirtualObjectKind::Shaped { shape },
            fields,
        },
    ));
    DeoptSlot::virtual_object(id)
}

fn deopt_slot(graph: &Graph, slots: SlotLayout, value: NodeId, location: Location) -> DeoptSlot {
    let repr = match graph.node(value).repr {
        Repr::Int32 => DeoptRepr::Int32,
        Repr::Float64 => DeoptRepr::Float64,
        _ => DeoptRepr::Tagged,
    };
    let location = match location {
        Location::Gp(register) => DeoptLocation::Register(register),
        Location::Fp(register) => {
            DeoptLocation::Register(otter_vm::deopt::DEOPT_FLOAT_REGISTER_BASE + register)
        }
        Location::TaggedSlot(_) | Location::UntaggedSlot(_) => {
            DeoptLocation::StackSlot(slots.offset(location) as i32)
        }
        Location::Constant(node) => match graph.node(node).kind {
            Kind::ConstTagged(bits) | Kind::ConstFloat64(bits) => DeoptLocation::Literal(bits),
            Kind::ConstInt32(int) => DeoptLocation::Literal(u64::from(int as u32)),
            _ => unreachable!("a constant"),
        },
    };
    DeoptSlot::physical(location, repr)
}

pub(crate) fn exit_reason(reason: DeoptReason) -> (ExitReason, ExitAction) {
    match reason {
        DeoptReason::WrongType => (ExitReason::TypeMismatch, ExitAction::Recompile),
        DeoptReason::WrongShape => (ExitReason::ShapeGuard, ExitAction::Recompile),
        DeoptReason::OwnDataMiss => (ExitReason::OwnDataMiss, ExitAction::Recompile),
        DeoptReason::WrongValue => (ExitReason::IdentityGuard, ExitAction::Recompile),
        DeoptReason::Overflow => (ExitReason::Int32Overflow, ExitAction::Recompile),
        DeoptReason::MinusZero => (ExitReason::NegativeZero, ExitAction::Recompile),
        DeoptReason::OutOfBounds => (ExitReason::BoundsGuard, ExitAction::Recompile),
        DeoptReason::InvalidIndex => (ExitReason::InvalidElementIndex, ExitAction::Recompile),
        DeoptReason::LostPrecision => (ExitReason::TypeMismatch, ExitAction::Recompile),
        DeoptReason::InsufficientFeedback => {
            (ExitReason::InsufficientFeedback, ExitAction::Recompile)
        }
        DeoptReason::Unsupported => (ExitReason::UnsupportedOperation, ExitAction::Resume),
        DeoptReason::DependencyChanged => (ExitReason::DependencyChanged, ExitAction::Recompile),
        DeoptReason::AllocationSiteDecided => {
            (ExitReason::AllocationSiteDecided, ExitAction::Recompile)
        }
    }
}
