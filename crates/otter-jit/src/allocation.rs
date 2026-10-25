//! Target-neutral recipes for generated allocation.
//!
//! # Contents
//! - [`LabRegisters`] names declared probe and initialization temporaries.
//! - [`EmptyLiteralLayout`] selects one VM-prepared shell layout.
//! - [`AllocationValue`] reads an explicit input from a constant, register or canonical tagged home.
//! - [`ConcatPlacement`] places the string one native concatenation creates.
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

use otter_vm::jit::{
    JitAllocationSite, JitEmptyArrayAllocationPlan, JitEmptyObjectAllocationPlan, JitSiteMode,
};

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

/// Where one native concatenation places the string it creates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConcatPlacement {
    /// The `+` site's placement when the code was compiled. An old cons
    /// owes its caller's write barrier for both child edges; a tracking
    /// site's cells are tagged and each birth is counted.
    Site(JitAllocationSite),
    /// Code shared by many sites: this register holds the site record's
    /// address, or zero for none, and its decision when the code runs
    /// places the string. A tenured site misses; a tracking one tags and
    /// counts; any other allocates plain young cells.
    Record(u8),
}

impl ConcatPlacement {
    /// Plain untagged young cells.
    #[cfg(test)]
    pub(crate) const YOUNG: Self = Self::Site(JitAllocationSite {
        id: 0,
        record: 0,
        mode: JitSiteMode::Young,
    });

    /// The compiled site, when the code bakes one.
    pub(crate) const fn site(self) -> Option<JitAllocationSite> {
        match self {
            Self::Site(site) => Some(site),
            Self::Record(_) => None,
        }
    }

    /// Whether the string is born old.
    pub(crate) fn old(self) -> bool {
        self.site()
            .is_some_and(|site| site.mode == JitSiteMode::Old)
    }

    /// The third operand of the collecting concatenation for `site`:
    /// `undefined`, or the site id as an Int32.
    pub(crate) fn stub_operand(site: JitAllocationSite) -> u64 {
        if site.id == 0 {
            crate::entry::VALUE_UNDEFINED
        } else {
            otter_vm::Value::number_i32(i32::from(site.id)).to_bits()
        }
    }

    /// The header word a new string is born with, from the plain young one.
    pub(crate) fn header_word(self, young: u64) -> u64 {
        self.site().map_or(young, |site| site.header_word(young))
    }
}
