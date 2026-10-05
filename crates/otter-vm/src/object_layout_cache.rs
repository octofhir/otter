//! Isolate-owned layouts for atomized host record signatures.
//!
//! Native parsers and marshallers often see the same record signature many
//! times: one record-kind atom followed by the same ordered property atoms.
//! This module retains complete [`ObjectLayout`] tokens keyed by the ordered
//! host atom identities and the owning realm's root shape.
//!
//! # Contents
//! - [`ObjectLayoutCache`] — signature-to-layout table.
//!
//! # Invariants
//! - Every cached shape belongs to the owning isolate and enters its root walk.
//! - Property order is part of the signature; tag identity partitions records
//!   that happen to expose the same keys, and the root shape of the realm's
//!   `%Object.prototype%` — which a layout's shape fixes — partitions realms.
//! - Cache keys contain no GC handles and need no root tracing.
//! - A hit borrows the caller's atom slice and performs no allocation.
//!
//! # See also
//! - [`crate::handles::ObjectLayout`] — stable hidden-class token.
//! - [`crate::host_strings::HostAtom`] — opaque host-side name identity.

use crate::handles::ObjectLayout;
use crate::host_strings::{HostAtom, HostAtomId};
use rustc_hash::FxHashMap;

#[derive(Debug)]
struct LayoutEntry {
    root: crate::object::ShapeId,
    keys: Box<[HostAtomId]>,
    layout: ObjectLayout,
}

/// Derived, non-GC layout state owned by one interpreter.
#[derive(Debug, Default)]
pub(crate) struct ObjectLayoutCache {
    layouts: FxHashMap<HostAtomId, Vec<LayoutEntry>>,
}

impl ObjectLayoutCache {
    /// Shape ids of every cached layout, for the root walk: a layout handed
    /// to an embedder must keep resolving.
    pub(crate) fn shape_ids(&self) -> impl Iterator<Item = crate::object::ShapeId> + '_ {
        self.layouts
            .values()
            .flat_map(|entries| entries.iter().map(|entry| entry.layout.shape_id()))
    }

    /// Return the complete layout for `tag + keys`, if already learned.
    ///
    /// The caller's atom slice is compared in place, so a hit allocates no
    /// temporary signature and never enters the isolate name interner.
    #[must_use]
    pub(crate) fn get(
        &self,
        root: crate::object::ShapeId,
        tag: &HostAtom,
        keys: &[&HostAtom],
    ) -> Option<ObjectLayout> {
        self.layouts.get(&tag.id())?.iter().find_map(|entry| {
            (entry.root == root
                && entry.keys.len() == keys.len()
                && entry
                    .keys
                    .iter()
                    .zip(keys)
                    .all(|(cached, key)| *cached == key.id()))
            .then_some(entry.layout)
        })
    }

    /// Remember one complete signature after its shape chain has been built.
    pub(crate) fn insert(
        &mut self,
        root: crate::object::ShapeId,
        tag: &HostAtom,
        keys: &[&HostAtom],
        layout: ObjectLayout,
    ) {
        let entries = self.layouts.entry(tag.id()).or_default();
        let keys: Box<[HostAtomId]> = keys.iter().map(|key| key.id()).collect();
        debug_assert!(
            entries
                .iter()
                .all(|entry| entry.root != root || entry.keys != keys)
        );
        entries.push(LayoutEntry { root, keys, layout });
    }
}
