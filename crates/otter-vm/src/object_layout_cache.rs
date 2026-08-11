//! Isolate-owned layouts for atomized host record signatures.
//!
//! Native parsers and marshallers often see the same record signature many
//! times: one record-kind atom followed by the same ordered property atoms.
//! This module bridges opaque host atoms to the isolate's property atoms once
//! and retains the resulting complete [`ObjectLayout`].
//!
//! # Contents
//! - [`ObjectLayoutCache`] — host-atom bridge and signature-to-layout table.
//!
//! # Invariants
//! - Every cached VM atom was minted by the owning isolate's `NameInterner`.
//! - Property order is part of the signature; tag identity partitions records
//!   that happen to expose the same keys.
//! - Cache keys contain no GC handles and need no root tracing.
//! - A hit borrows the caller's atom slice and performs no allocation.
//!
//! # See also
//! - [`crate::handles::ObjectLayout`] — stable hidden-class token.
//! - [`crate::host_strings::HostAtom`] — opaque host-side name identity.

use crate::handles::ObjectLayout;
use crate::host_strings::{HostAtom, HostAtomId};
use crate::property_atom::{AtomId, NameInterner};
use rustc_hash::FxHashMap;

#[derive(Debug)]
struct LayoutEntry {
    keys: Box<[HostAtomId]>,
    layout: ObjectLayout,
}

/// Derived, non-GC layout state owned by one interpreter.
#[derive(Debug, Default)]
pub(crate) struct ObjectLayoutCache {
    host_atoms: FxHashMap<HostAtomId, AtomId>,
    layouts: FxHashMap<HostAtomId, Vec<LayoutEntry>>,
}

impl ObjectLayoutCache {
    /// Translate a host atom into this isolate's permanent property atom.
    pub(crate) fn atom_id(&mut self, names: &NameInterner, atom: &HostAtom) -> AtomId {
        if let Some(id) = self.host_atoms.get(&atom.id()) {
            return *id;
        }
        let id = names.intern(atom.as_str());
        self.host_atoms.insert(atom.id(), id);
        id
    }

    /// Return the complete layout for `tag + keys`, if already learned.
    ///
    /// The caller's atom slice is compared in place, so a hit allocates no
    /// temporary signature and never enters the isolate name interner.
    #[must_use]
    pub(crate) fn get(&self, tag: &HostAtom, keys: &[&HostAtom]) -> Option<ObjectLayout> {
        self.layouts.get(&tag.id())?.iter().find_map(|entry| {
            (entry.keys.len() == keys.len()
                && entry
                    .keys
                    .iter()
                    .zip(keys)
                    .all(|(cached, key)| *cached == key.id()))
            .then_some(entry.layout)
        })
    }

    /// Remember one complete signature after its shape chain has been built.
    pub(crate) fn insert(&mut self, tag: &HostAtom, keys: &[&HostAtom], layout: ObjectLayout) {
        let entries = self.layouts.entry(tag.id()).or_default();
        let keys: Box<[HostAtomId]> = keys.iter().map(|key| key.id()).collect();
        debug_assert!(entries.iter().all(|entry| entry.keys != keys));
        entries.push(LayoutEntry { keys, layout });
    }
}
