//! A function's source spans as one compact, delta-encoded byte table.
//!
//! V8 keeps source positions the same way (`SourcePositionTable`): the
//! entries are written once, read by iteration when a position is actually
//! asked for, and never expanded into a record per instruction. Most compiled
//! functions never report a position, so neither the cache decoder nor a
//! linked module pays for them.
//!
//! # Contents
//! - [`SpanTable`] — the encoded entries with their count.
//! - [`SpanIter`] — decodes entries in order.
//!
//! # Invariants
//! - Each entry is three zigzag LEB128 deltas: PC from the previous PC, start
//!   from the previous start, end from this start. The first entry is relative
//!   to `(0, 0)`.
//! - Iteration stops at the first entry that does not decode; only
//!   [`SpanTable::well_formed`] tells a complete table from a truncated one,
//!   so a table from untrusted bytes is checked before it is trusted.
//!
//! # See also
//! - [`crate::binary`] stores the table bytes verbatim.
//! - [`crate::verifier`] checks a decoded function's table is well formed and
//!   in PC order.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::SpanEntry;

/// One function's `(pc, span)` entries, delta-encoded.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpanTable {
    count: u32,
    bytes: Box<[u8]>,
}

impl SpanTable {
    /// Encode `entries` in order.
    ///
    /// # Panics
    /// Panics when there are more than `u32::MAX` entries.
    #[must_use]
    pub fn new(entries: &[SpanEntry]) -> Self {
        let mut bytes = Vec::with_capacity(entries.len() * 3);
        let (mut pc, mut start) = (0, 0);
        for entry in entries {
            push_delta(&mut bytes, pc, entry.pc);
            push_delta(&mut bytes, start, entry.span.0);
            push_delta(&mut bytes, entry.span.0, entry.span.1);
            pc = entry.pc;
            start = entry.span.0;
        }
        Self {
            count: u32::try_from(entries.len()).expect("span count fits u32"),
            bytes: bytes.into_boxed_slice(),
        }
    }

    /// A table from its stored parts, unchecked; see [`Self::well_formed`].
    #[must_use]
    pub fn from_parts(count: u32, bytes: Box<[u8]>) -> Self {
        Self { count, bytes }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether the table has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The encoded entries.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Decode the entries in order.
    #[must_use]
    pub fn iter(&self) -> SpanIter<'_> {
        SpanIter {
            bytes: &self.bytes,
            remaining: self.count,
            pc: 0,
            start: 0,
        }
    }

    /// Whether exactly `len()` entries decode and consume every byte.
    #[must_use]
    pub fn well_formed(&self) -> bool {
        let mut iter = self.iter();
        for _ in 0..self.count {
            if iter.next().is_none() {
                return false;
            }
        }
        iter.bytes.is_empty()
    }

    /// Heap bytes this table retains.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

impl std::fmt::Debug for SpanTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl FromIterator<SpanEntry> for SpanTable {
    fn from_iter<I: IntoIterator<Item = SpanEntry>>(entries: I) -> Self {
        Self::new(&entries.into_iter().collect::<Vec<_>>())
    }
}

impl<'a> IntoIterator for &'a SpanTable {
    type Item = SpanEntry;
    type IntoIter = SpanIter<'a>;

    fn into_iter(self) -> SpanIter<'a> {
        self.iter()
    }
}

impl Serialize for SpanTable {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for SpanTable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::new(&Vec::<SpanEntry>::deserialize(deserializer)?))
    }
}

/// Entries of a [`SpanTable`], decoded in order.
#[derive(Debug, Clone)]
pub struct SpanIter<'a> {
    bytes: &'a [u8],
    remaining: u32,
    pc: u32,
    start: u32,
}

impl SpanIter<'_> {
    fn delta(&mut self, from: u32) -> Option<u32> {
        let mut zigzag = 0u64;
        let mut shift = 0;
        loop {
            let (&byte, rest) = self.bytes.split_first()?;
            self.bytes = rest;
            if shift > 63 {
                return None;
            }
            zigzag |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let difference = ((zigzag >> 1) as i64) ^ -((zigzag & 1) as i64);
        u32::try_from(i64::from(from).checked_add(difference)?).ok()
    }
}

impl Iterator for SpanIter<'_> {
    type Item = SpanEntry;

    fn next(&mut self) -> Option<SpanEntry> {
        if self.remaining == 0 {
            return None;
        }
        let entry = (|| {
            let pc = self.delta(self.pc)?;
            let start = self.delta(self.start)?;
            let end = self.delta(start)?;
            Some(SpanEntry {
                pc,
                span: (start, end),
            })
        })();
        match entry {
            Some(entry) => {
                self.remaining -= 1;
                self.pc = entry.pc;
                self.start = entry.span.0;
                Some(entry)
            }
            None => {
                self.remaining = 0;
                None
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.remaining as usize))
    }
}

/// Append `to - from` as a zigzag LEB128 varint.
fn push_delta(bytes: &mut Vec<u8>, from: u32, to: u32) {
    let difference = i64::from(to) - i64::from(from);
    let mut zigzag = ((difference << 1) ^ (difference >> 63)) as u64;
    while zigzag >= 0x80 {
        bytes.push(zigzag as u8 | 0x80);
        zigzag >>= 7;
    }
    bytes.push(zigzag as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pc: u32, start: u32, end: u32) -> SpanEntry {
        SpanEntry {
            pc,
            span: (start, end),
        }
    }

    #[test]
    fn entries_round_trip_including_backward_deltas() {
        let entries = [
            entry(0, 10, 12),
            entry(3, 4, 4),
            entry(3, u32::MAX - 1, u32::MAX),
            entry(u32::MAX, 0, 7),
        ];
        let table = SpanTable::new(&entries);
        assert!(table.well_formed());
        assert_eq!(table.len(), entries.len());
        assert_eq!(table.iter().collect::<Vec<_>>(), entries);
        let copy = SpanTable::from_parts(entries.len() as u32, table.as_bytes().into());
        assert_eq!(copy, table);
    }

    #[test]
    fn truncated_or_padded_bytes_are_not_well_formed() {
        let table = SpanTable::new(&[entry(1, 2, 3), entry(4, 5, 6)]);
        let bytes = table.as_bytes();
        let truncated = SpanTable::from_parts(2, bytes[..bytes.len() - 1].into());
        assert!(!truncated.well_formed());
        assert_eq!(truncated.iter().count(), 1);
        let mut padded = bytes.to_vec();
        padded.push(0);
        assert!(!SpanTable::from_parts(2, padded.into()).well_formed());
        assert!(!SpanTable::from_parts(3, bytes.into()).well_formed());
    }
}
