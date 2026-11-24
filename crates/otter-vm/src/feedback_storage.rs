//! Packed instruction-to-feedback indices and address-stable feedback banks.
//!
//! # Contents
//! Opcode selection, exact allocation geometry, and typed bank lookup.
//!
//! # Invariants
//! A method instruction owns a property slot and a call slot under one index.
//! Property and call payloads are boxed together by family before publication.
//! Their addresses stay fixed for the lifetime of the enclosing CodeBlock.
//! Clones for a fresh isolate build empty banks from the same bytecode.
//!
//! # See also
//! [`super::FeedbackVector`] owns the banks and their transition epoch.

use super::{AtomicCallFeedback, InstructionFeedback, PropertyIcKind, PropertyIcSlot};
use crate::executable::allocation::{array_bytes, try_vec};
use otter_bytecode::Op;
use otter_resource::{ResourceError, ResourceLease};
use std::sync::atomic::AtomicU16;

#[derive(Clone, Copy)]
enum SlotKind {
    None,
    Property(PropertyIcKind),
    Method,
    Call,
    Allocation,
}

impl SlotKind {
    fn of(op: Op) -> Self {
        match op {
            Op::LoadProperty | Op::HasNamedProperty | Op::LoadElement => {
                Self::Property(PropertyIcKind::Load)
            }
            Op::StoreProperty
            | Op::StorePropertyStrict
            | Op::StoreElement
            | Op::StoreElementStrict => Self::Property(PropertyIcKind::Store),
            Op::CallMethodValue => Self::Method,
            Op::Call
            | Op::TailCall
            | Op::CallWithThis
            | Op::CallForwardArguments
            | Op::CallSpread
            | Op::New
            | Op::NewSpread
            | Op::SuperConstruct
            | Op::SuperConstructSpread => Self::Call,
            Op::NewObjectLiteral
            | Op::NewArray
            | Op::NewArrayBoilerplate
            | Op::Add
            | Op::AddImm => Self::Allocation,
            _ => Self::None,
        }
    }
}

/// Geometry admitted before any bank is allocated.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FeedbackLayout {
    instructions: usize,
    properties: u32,
    methods: u32,
    calls: u32,
    allocations: u32,
}

impl FeedbackLayout {
    pub(crate) fn for_ops(ops: impl IntoIterator<Item = Op>) -> Self {
        let mut result = Self::default();
        for op in ops {
            result.count(op);
        }
        result
    }

    pub(crate) fn count(&mut self, op: Op) {
        self.instructions += 1;
        match SlotKind::of(op) {
            SlotKind::None => {}
            SlotKind::Property(_) => self.properties += 1,
            SlotKind::Method => self.methods += 1,
            SlotKind::Call => self.calls += 1,
            SlotKind::Allocation => self.allocations += 1,
        }
    }

    pub(crate) fn bytes(self) -> u64 {
        array_bytes::<InstructionFeedback>(self.instructions)
            .saturating_add(array_bytes::<u32>(self.instructions))
            .saturating_add(array_bytes::<PropertyIcSlot>(self.properties as usize))
            .saturating_add(array_bytes::<MethodSlot>(self.methods as usize))
            .saturating_add(array_bytes::<AtomicCallFeedback>(self.calls as usize))
            .saturating_add(array_bytes::<AtomicU16>(self.allocations as usize))
    }

    fn methods_end(self) -> u32 {
        self.properties + self.methods
    }

    fn calls_end(self) -> u32 {
        self.methods_end() + self.calls
    }
}

#[derive(Debug)]
struct MethodSlot {
    property: PropertyIcSlot,
    call: AtomicCallFeedback,
}

/// Dense physical payloads with one 32-bit slot id per canonical instruction.
/// Slot ids are consecutive over the property, method, call and allocation
/// families in that order. An index for a missing slot is u32::MAX.
#[derive(Debug, Default)]
pub(super) struct FeedbackBanks {
    indices: Box<[u32]>,
    properties: Box<[PropertyIcSlot]>,
    methods: Box<[MethodSlot]>,
    calls: Box<[AtomicCallFeedback]>,
    allocations: Box<[AtomicU16]>,
    layout: FeedbackLayout,
}

impl FeedbackBanks {
    pub(super) fn new(
        ops: impl IntoIterator<Item = Op>,
        layout: FeedbackLayout,
        lease: &mut ResourceLease,
    ) -> Result<Self, ResourceError> {
        let mut indices = try_vec(layout.instructions, lease)?;
        let mut properties = try_vec(layout.properties as usize, lease)?;
        let mut methods = try_vec(layout.methods as usize, lease)?;
        let mut calls = try_vec(layout.calls as usize, lease)?;
        let mut allocations = try_vec(layout.allocations as usize, lease)?;
        for op in ops {
            indices.push(match SlotKind::of(op) {
                SlotKind::None => u32::MAX,
                SlotKind::Property(kind) => {
                    let index = properties.len() as u32;
                    properties.push(PropertyIcSlot::new(kind));
                    index
                }
                SlotKind::Method => {
                    let index = layout.properties + methods.len() as u32;
                    methods.push(MethodSlot {
                        property: PropertyIcSlot::new(PropertyIcKind::Load),
                        call: AtomicCallFeedback::default(),
                    });
                    index
                }
                SlotKind::Call => {
                    let index = layout.methods_end() + calls.len() as u32;
                    calls.push(AtomicCallFeedback::default());
                    index
                }
                SlotKind::Allocation => {
                    let index = layout.calls_end() + allocations.len() as u32;
                    allocations.push(AtomicU16::new(0));
                    index
                }
            });
        }
        debug_assert_eq!(indices.len(), layout.instructions);
        debug_assert_eq!(properties.len(), layout.properties as usize);
        debug_assert_eq!(methods.len(), layout.methods as usize);
        debug_assert_eq!(calls.len(), layout.calls as usize);
        debug_assert_eq!(allocations.len(), layout.allocations as usize);
        Ok(Self {
            indices: indices.into_boxed_slice(),
            properties: properties.into_boxed_slice(),
            methods: methods.into_boxed_slice(),
            calls: calls.into_boxed_slice(),
            allocations: allocations.into_boxed_slice(),
            layout,
        })
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        self.layout
            .bytes()
            .saturating_sub(array_bytes::<InstructionFeedback>(self.indices.len()))
    }

    pub(super) fn property(&self, pc: usize) -> Option<&PropertyIcSlot> {
        let index = *self.indices.get(pc)?;
        if index < self.layout.properties {
            return self.properties.get(index as usize);
        }
        self.methods
            .get((index - self.layout.properties) as usize)
            .map(|slot| &slot.property)
    }

    #[cfg(test)]
    pub(super) fn is_method(&self, pc: usize) -> bool {
        self.indices.get(pc).is_some_and(|index| {
            (self.layout.properties..self.layout.methods_end()).contains(index)
        })
    }

    pub(super) fn call(&self, pc: usize) -> Option<&AtomicCallFeedback> {
        let index = self.indices.get(pc)?.checked_sub(self.layout.properties)?;
        if index < self.layout.methods {
            return self.methods.get(index as usize).map(|slot| &slot.call);
        }
        self.calls.get((index - self.layout.methods) as usize)
    }

    pub(super) fn allocation(&self, pc: usize) -> Option<&AtomicU16> {
        let index = self.indices.get(pc)?.checked_sub(self.layout.calls_end())?;
        self.allocations.get(index as usize)
    }

    pub(super) fn properties(&self) -> impl Iterator<Item = &PropertyIcSlot> {
        self.properties
            .iter()
            .chain(self.methods.iter().map(|slot| &slot.property))
    }
}
