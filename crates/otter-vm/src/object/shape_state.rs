//! Immutable lookup and allocation facts owned by each hidden class.
//!
//! # Contents
//! - `ShapeState` is the sole packed engine state carried by `ShapeBody`.
//! - `LookupFact` identifies lookup semantics outside ordinary named slots.
//!
//! # Invariants
//! - Object headers carry no lookup, extensibility, descriptor or prototype bits.
//! - Attribute authority is shape or dictionary metadata, never a state latch.
//! - A provisional lineage is an ordinary immutable layout for every guard,
//!   runtime or generated; only allocation plans refuse a provisional root.
//! - Capacity is separate immutable geometry; changing this state never changes it.
//!
//! # See also
//! - `super::shape_body` stores this byte and copies it through every descendant.
//! - `super::state_transition` prepares rooted variants before publication.

/// Lookup semantics which ordinary named-slot proofs cannot describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LookupFact {
    /// A String wrapper has virtual indexed and length properties.
    StringWrapper,
    /// Mapped arguments can alias parameter-context cells.
    MappedArguments,
    /// A host payload can substitute named lookup results.
    HostLookup,
    /// A non-ordinary prototype requires its canonical lookup owner.
    NonOrdinaryPrototype,
}

/// Scalar immutable facts shared by all objects carrying one hidden class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct ShapeState(u8);

impl ShapeState {
    /// Dictionary metadata owns descriptors and key order.
    pub const DICTIONARY_MASK: u8 = 0x01;
    /// New properties are permitted.
    pub const EXTENSIBLE_MASK: u8 = 0x02;
    /// Semantic mutation must retire dependent prototype proofs.
    pub const PROTOTYPE_MASK: u8 = 0x04;
    /// String wrapper lookup fact.
    pub const STRING_WRAPPER_MASK: u8 = 0x08;
    /// Mapped-arguments lookup fact.
    pub const MAPPED_ARGUMENTS_MASK: u8 = 0x10;
    /// Host lookup fact.
    pub const HOST_LOOKUP_MASK: u8 = 0x20;
    /// Non-ordinary prototype lookup fact.
    pub const OPAQUE_PROTOTYPE_MASK: u8 = 0x40;
    /// The constructor layout is still being sampled.
    pub const PROVISIONAL_MASK: u8 = 0x80;
    /// Facts excluding ordinary named-slot lookup.
    pub const OPAQUE_LOOKUP_MASK: u8 = 0x78;
    /// A fresh ordinary extensible lineage.
    pub const ORDINARY: Self = Self(Self::EXTENSIBLE_MASK);

    /// Physical scalar byte read by generated dynamic probes.
    pub const fn bits(self) -> u8 {
        self.0
    }
    /// Whether dictionary metadata owns the properties.
    pub const fn is_dictionary(self) -> bool {
        self.0 & Self::DICTIONARY_MASK != 0
    }
    /// Whether the object permits new properties.
    pub const fn is_extensible(self) -> bool {
        self.0 & Self::EXTENSIBLE_MASK != 0
    }
    /// Whether the object has acquired prototype role.
    pub const fn is_prototype(self) -> bool {
        self.0 & Self::PROTOTYPE_MASK != 0
    }
    /// Whether ordinary named-slot lookup is insufficient.
    pub const fn is_opaque(self) -> bool {
        self.0 & Self::OPAQUE_LOOKUP_MASK != 0
    }
    /// Whether this lineage descends from a constructor root still being
    /// sampled. Such a root never serves an allocation plan.
    pub const fn is_provisional(self) -> bool {
        self.0 & Self::PROVISIONAL_MASK != 0
    }
    /// Select descriptor/key storage without changing other facts.
    pub const fn with_dictionary(self, on: bool) -> Self {
        self.with_mask(Self::DICTIONARY_MASK, on)
    }
    /// Select extensibility without changing other facts.
    pub const fn with_extensible(self, on: bool) -> Self {
        self.with_mask(Self::EXTENSIBLE_MASK, on)
    }
    /// Preserve or acquire prototype role.
    pub const fn with_prototype_role(self, on: bool) -> Self {
        self.with_mask(Self::PROTOTYPE_MASK, on)
    }
    /// Set one typed lookup fact without a general flag mutation API.
    pub const fn with_lookup(self, fact: LookupFact, on: bool) -> Self {
        let mask = match fact {
            LookupFact::StringWrapper => Self::STRING_WRAPPER_MASK,
            LookupFact::MappedArguments => Self::MAPPED_ARGUMENTS_MASK,
            LookupFact::HostLookup => Self::HOST_LOOKUP_MASK,
            LookupFact::NonOrdinaryPrototype => Self::OPAQUE_PROTOTYPE_MASK,
        };
        self.with_mask(mask, on)
    }
    /// Select layout sampling for a newly allocated lineage.
    pub const fn with_provisional(self, on: bool) -> Self {
        self.with_mask(Self::PROVISIONAL_MASK, on)
    }
    const fn with_mask(self, mask: u8, on: bool) -> Self {
        Self(if on { self.0 | mask } else { self.0 & !mask })
    }
}

#[cfg(test)]
mod tests;
