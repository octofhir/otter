//! Descriptor-proof retirement and in-place object integrity changes.
//!
//! # Contents
//! - [`DescriptorChanges`] aggregates single-slot and bulk descriptor changes.
//! - [`IntegrityLevel`] supplies sealed/frozen property flags.
//! - [`apply_integrity_level`] retires proofs before publishing integrity flags.
//!
//! # Invariants
//! - A changed dictionary descriptor set handled here receives one fresh
//!   structural identity. Its watched-slot layout epoch advances once only if
//!   a changed string slot was watched; a saturated epoch remains unprovable.
//! - Equal descriptors preserve identities and watched slots, including when
//!   their data values change. A changed slot stops carrying its retired watch.
//! - Retirement precedes descriptor/value publication and never allocates.
//!   The caller prepares non-extensible immutable state before this owner.
//! - Integrity changes skip Private Names and preserve accessor semantics.
//!   All metadata tables are reserved before entering this noncollecting owner.
//!
//! # See also
//! - `super::ObjectBody::set_slot` uses the same single-slot retirement owner.
//! - `super::watch_dictionary_slot` owns the direct-slot proof premise.
//! - `crate::interp::shapes` prepares immutable shaped integrity transitions.

use super::{ObjectBody, PropertyFlags, SlotMeta, next_shape_id};

/// Aggregate proof domains invalidated by one descriptor mutation.
#[derive(Clone, Copy, Default)]
pub(super) struct DescriptorChanges {
    changed: bool,
    watched_layout: bool,
}

impl DescriptorChanges {
    pub(super) fn for_slot(previous: SlotMeta, next: SlotMeta) -> Self {
        let mut changes = Self::default();
        changes.note_slot(previous, next.flags, next.is_accessor);
        changes
    }

    fn note_slot(&mut self, previous: SlotMeta, flags: PropertyFlags, is_accessor: bool) {
        let changed = previous.flags != flags || previous.is_accessor != is_accessor;
        self.changed |= changed;
        self.watched_layout |= changed && previous.watched;
    }

    pub(super) fn changed(self) -> bool {
        self.changed
    }

    /// Retire the affected proof domains before the caller publishes any slot.
    pub(super) fn retire(self, body: &mut ObjectBody) {
        if !self.changed {
            return;
        }
        body.invalidate_prototype_proofs();
        if body.is_dictionary() {
            body.exotic_mut().dictionary_shape_id = next_shape_id();
            if self.watched_layout {
                body.advance_dictionary_layout();
            }
        }
    }
}

/// The two ordinary-object integrity levels.
#[derive(Clone, Copy)]
pub(super) enum IntegrityLevel {
    Sealed,
    Frozen,
}

impl IntegrityLevel {
    fn flags(self, flags: PropertyFlags, is_accessor: bool) -> PropertyFlags {
        let flags = flags.with_configurable(false);
        match self {
            Self::Frozen if !is_accessor => flags.with_writable(false),
            _ => flags,
        }
    }
}

/// Update materialized descriptors and symbols after retiring their old proof.
pub(super) fn apply_integrity_level(body: &mut ObjectBody, level: IntegrityLevel) {
    let mut changes = DescriptorChanges::default();
    for &slot in body.slots() {
        changes.note_slot(
            slot,
            level.flags(slot.flags, slot.is_accessor),
            slot.is_accessor,
        );
    }
    for (key, slot) in body.symbol_props() {
        // Private Names are internal slots, not ordinary properties.
        if !key.is_private_name() {
            changes.changed |= slot.flags != level.flags(slot.flags, !slot.kind.is_data());
        }
    }
    changes.retire(body);

    if !body.slots().is_empty() {
        for slot in body.slots_mut().entries_mut() {
            let flags = level.flags(slot.flags, slot.is_accessor);
            if slot.flags != flags {
                slot.watched = false;
                slot.flags = flags;
            }
        }
    }
    if let Some(symbols) = body.symbol_props_mut() {
        for (key, slot) in symbols.entries_mut() {
            if !key.is_private_name() {
                slot.flags = level.flags(slot.flags, !slot.kind.is_data());
            }
        }
    }
}

#[cfg(test)]
mod tests;
