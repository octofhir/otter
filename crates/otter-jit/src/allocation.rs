//! Target-neutral recipes for generated allocation.
//!
//! # Contents
//! - [`LabRegisters`] names declared probe and initialization temporaries.
//! - [`EmptyLiteralLayout`] selects one VM-prepared shell layout.
//! - [`AllocationValue`] reads an explicit input from a constant, register or canonical tagged home.
//!
//! # Invariants
//! - Recipes contain no runtime state, pointer or target instruction encoding.
//! - The candidate is a temporary; the caller commits the result after a fit.
//! - Stack bytes are relative to the current native stack reservation.
//! - Each target owns one LAB probe, initialization and publication encoder.
//!
//! # See also
//! - `otter_vm::jit::JitLiteralAllocationPlans` owns exact cell geometry.
//! - [`crate::graph::call`] owns the committed collecting miss operands.

use otter_vm::jit::{JitEmptyArrayAllocationPlan, JitEmptyObjectAllocationPlan};

/// Registers owned by one nursery probe, initialization and publication.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LabRegisters {
    pub(crate) buffer: u8,
    pub(crate) candidate: u8,
    pub(crate) end: u8,
    pub(crate) scratch: u8,
    pub(crate) size: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmptyLiteralLayout {
    Object(JitEmptyObjectAllocationPlan),
    Array(JitEmptyArrayAllocationPlan),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AllocationValue {
    StackByte(u32),
    Constant(u64),
    /// A Template operand register outside the allocator temporary set.
    Register(u8),
}

#[cfg(test)]
#[path = "allocation/lexical_tests.rs"]
mod lexical_tests;

#[cfg(test)]
#[path = "allocation/derived_context_tests.rs"]
mod derived_context_tests;

impl EmptyLiteralLayout {
    pub(crate) fn bytes(self) -> u32 {
        match self {
            Self::Object(p) => p.cell_bytes,
            Self::Array(p) => p.cell_bytes,
        }
    }
    pub(crate) fn header(self) -> u64 {
        match self {
            Self::Object(p) => p.header_word,
            Self::Array(p) => p.header_word,
        }
    }
}

#[cfg(test)]
#[path = "allocation/string_group_tests.rs"]
mod string_group_tests;
