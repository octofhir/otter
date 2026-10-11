//! Owned reconstruction records for foreign Array indexed descriptor storage.
//!
//! # Contents
//! - The current snapshot record for scalar length and numeric attributes.
//! - Capture and reconstruction of the sidecar's host allocation.
//!
//! # Invariants
//! - The managed expando and all its edges use the page image and relocation.
//! - Numeric records are sorted and unique; no named-property mirror exists.
//! - Sparse, accessor and JSON host payloads remain snapshot capture refusals.
//!
//! # See also
//! - [`crate::snapshot`] captures one record per Array sidecar.
//! - [`crate::interp::restore`] rebuilds severed host ownership.

use super::{ArrayExoticSlots, PropertyFlags};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub(crate) struct ArrayIndexedSnapshot {
    pub(crate) length_writable: bool,
    pub(crate) property_flags: Vec<(u32, PropertyFlags)>,
}

impl ArrayExoticSlots {
    pub(crate) fn indexed_snapshot(&self) -> ArrayIndexedSnapshot {
        let mut property_flags: Vec<_> = self
            .property_flags
            .as_ref()
            .map(|flags| flags.iter().map(|(&key, &flags)| (key, flags)).collect())
            .unwrap_or_default();
        property_flags.sort_unstable_by_key(|&(key, _)| key);
        ArrayIndexedSnapshot {
            length_writable: !self.length_nonwritable,
            property_flags,
        }
    }

    pub(crate) fn restore_indexed_snapshot(&mut self, record: &ArrayIndexedSnapshot) {
        self.length_nonwritable = !record.length_writable;
        self.property_flags = (!record.property_flags.is_empty()).then(|| {
            record
                .property_flags
                .iter()
                .copied()
                .collect::<HashMap<_, _>>()
        });
    }
}
