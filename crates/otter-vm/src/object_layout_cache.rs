//! Isolate-owned layouts for fixed and atomized host records.
//!
//! Native parsers and marshallers often see the same record signature many
//! times: one record-kind atom followed by the same ordered property atoms.
//! This module retains complete [`ObjectLayout`] tokens. Fixed null-prototype
//! RegExp records use typed slots; dynamic signatures use the ordered host
//! atom identities and the selected prototype root shape.
//!
//! # Contents
//! - [`RegExpRecordLayout`] — the three fixed null-prototype signatures.
//! - [`ObjectLayoutCache`] — their slots and the dynamic signature table.
//!
//! # Invariants
//! - Every cached shape belongs to the owning isolate and enters its root walk.
//! - Property order is part of the signature; tag identity partitions records
//!   that happen to expose the same keys. The root partitions ordinary realm
//!   prototypes and null-prototype bags without a second cache.
//! - Cache keys contain no GC handles and need no root tracing.
//! - A dynamic hit borrows the caller's atom slice without allocation. A fixed
//!   hit copies its retained token without atoms, root preparation or hashing.
//! - Fixed slots contain no realm prototype or dynamic signature entry. Their
//!   final shapes enter the same root walk as dynamic layouts.
//!
//! # See also
//! - [`crate::handles::ObjectLayout`] — stable hidden-class token.
//! - [`crate::host_strings::HostAtom`] — opaque host-side name identity.

use crate::handles::ObjectLayout;
use crate::host_strings::{HostAtom, HostAtomId};
use rustc_hash::FxHashMap;

/// Fixed null-prototype records owned by the isolate's RegExp result builder.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(crate) enum RegExpRecordLayout {
    /// Match metadata without an indices array.
    Match,
    /// Match metadata with an indices array.
    MatchWithIndices,
    /// The named-group property of an indices array.
    Indices,
}

impl RegExpRecordLayout {
    /// Final property order, with default writable/enumerable/configurable data attributes.
    pub(crate) const fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Match => &["index", "input", "groups"],
            Self::MatchWithIndices => &["index", "input", "groups", "indices"],
            Self::Indices => &["groups"],
        }
    }
}

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
    regexp: [Option<ObjectLayout>; 3],
}

impl ObjectLayoutCache {
    /// Shape ids of every cached layout, for the root walk: a layout handed
    /// to an embedder must keep resolving.
    pub(crate) fn shape_ids(&self) -> impl Iterator<Item = crate::object::ShapeId> + '_ {
        self.layouts
            .values()
            .flat_map(|entries| entries.iter().map(|entry| entry.layout.shape_id()))
            .chain(self.regexp.iter().flatten().map(ObjectLayout::shape_id))
    }

    /// Read a fixed token before any root preparation or name lookup.
    #[must_use]
    pub(crate) fn regexp(&self, kind: RegExpRecordLayout) -> Option<ObjectLayout> {
        self.regexp[kind as usize]
    }

    /// Publish one completed null-prototype lineage solely in its typed slot.
    pub(crate) fn insert_regexp(&mut self, kind: RegExpRecordLayout, layout: ObjectLayout) {
        debug_assert!(self.regexp[kind as usize].is_none());
        debug_assert_eq!(layout.len(), kind.keys().len());
        self.regexp[kind as usize] = Some(layout);
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

#[cfg(test)]
mod tests;
