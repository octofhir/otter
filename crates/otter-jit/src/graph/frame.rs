//! Canonical Graph frame geometry and deoptimization recipes for every backend.
//!
//! # Contents
//! - [`SlotLayout`] places tagged homes, exception scratch and untagged homes.
//! - [`deopt_frames`] maps complete inline states to the VM's recovery frames.
//! - [`exit_reason`] selects the shared semantic exit and recovery action.
//!
//! # Invariants
//! - Only initialized tagged words are collector roots; unboxed homes follow.
//! - Reconstruction reads canonical homes, constants, or the machine registers
//!   the shared exit handler saves in its register dump.
//! - A recipe lists only registers whose value is not the literal `undefined`;
//!   every other register resumes `undefined`.
//! - Byte offsets fit the signed VM recipe and the native frame's root indices.
//! - Backend encoders consume this geometry without owning a second frame map.
//!
//! # See also
//! - [`super::regalloc`] for final physical homes and representation ownership.
//! - `otter_vm::deopt` for the one interpreter reconstruction contract.

use otter_vm::deopt::{DeoptFrame, DeoptFrameEntry, DeoptLocation, DeoptRepr, DeoptSlot};
use otter_vm::{
    native_abi::{self, ExitAction, ExitReason},
    value::tag::VALUE_UNDEFINED,
};

use super::ir::{DeoptReason, FrameStateId, Graph, Kind, NodeId, Repr};
use super::regalloc::{Allocation, Location};
use crate::Unsupported;

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
) -> Box<[DeoptFrame]> {
    let mut locations = locations.iter().copied();
    let mut slot = |value: NodeId| {
        let location = locations.next().expect("a location per state value");
        deopt_slot(graph, slots, value, location)
    };
    let frames = graph
        .state_chain(state)
        .into_iter()
        .map(|state| {
            let data = graph.frame_state(state);
            let entry = data.caller.map(|caller| DeoptFrameEntry {
                return_register: caller.return_register,
                this: slot(caller.this),
                closure: slot(caller.closure),
                new_target: slot(caller.new_target),
            });
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
            }
        })
        .collect();
    assert!(
        locations.next().is_none(),
        "one canonical location per reconstruction value"
    );
    frames
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
    }
}
