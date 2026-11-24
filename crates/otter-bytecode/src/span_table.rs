//! A function's source spans as one compact, delta-encoded byte table.
//!
//! The entries are written once, read by iteration when a position is actually
//! asked for, and never expanded into a record per instruction. Most compiled
//! functions never report a position, so neither the cache decoder nor a
//! linked module pays for them.
//!
//! # Contents
//! - [`SpanTable`] — the encoded entries with their count.
//! - [`SpanIter`] — decodes entries in order.
//!
//! # Invariants
//! - An entry is three zigzag LEB128 deltas: PC and start from the previous
//!   entry, end from this start. Each block of 64 entries begins at (0, 0).
//! - A trailer gives the first logical PC and byte offset for each block
//!   after the first; its size follows from the entry count. Sequential
//!   verification checks every checkpoint against the actual stream.
//! - An untrusted table must pass [`SpanTable::well_formed`] and the verifier's
//!   logical-PC checks before execution relies on it.
//!
//! # See also
//! - [`crate::binary`] stores the table bytes verbatim.
//! - [`crate::verifier`] checks a decoded function's table is well formed and
//!   in PC order.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::SpanEntry;

const BLOCK_ENTRIES: usize = 64;
const CHECKPOINT_BYTES: usize = 12;
type Checkpoint = [u8; CHECKPOINT_BYTES];

fn checkpoint_pc(checkpoint: &Checkpoint) -> u32 {
    u32::from_le_bytes(checkpoint[..4].try_into().expect("checkpoint PC"))
}

fn checkpoint_offset(checkpoint: &Checkpoint) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(
        checkpoint[4..].try_into().expect("checkpoint offset"),
    ))
    .ok()
}

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
        Self::encode(entries.iter().copied(), entries.len())
    }

    fn encode(entries: impl Iterator<Item = SpanEntry>, capacity: usize) -> Self {
        let mut bytes = Vec::with_capacity(capacity.saturating_mul(3));
        let mut checkpoints = Vec::new();
        let (mut pc, mut start, mut count) = (0, 0, 0usize);
        for entry in entries {
            if count != 0 && count.is_multiple_of(BLOCK_ENTRIES) {
                checkpoints.extend_from_slice(&entry.pc.to_le_bytes());
                checkpoints.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
                (pc, start) = (0, 0);
            }
            push_delta(&mut bytes, pc, entry.pc);
            push_delta(&mut bytes, start, entry.span.0);
            push_delta(&mut bytes, entry.span.0, entry.span.1);
            pc = entry.pc;
            start = entry.span.0;
            count += 1;
        }
        bytes.extend_from_slice(&checkpoints);
        Self {
            count: u32::try_from(count).expect("span count fits u32"),
            bytes: bytes.into_boxed_slice(),
        }
    }

    /// Transform source coordinates while keeping logical PCs unchanged.
    #[must_use]
    pub fn map_spans(&self, mut map: impl FnMut((u32, u32)) -> (u32, u32)) -> Self {
        Self::encode(
            self.iter().map(|entry| SpanEntry {
                pc: entry.pc,
                span: map(entry.span),
            }),
            self.len(),
        )
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

    fn parts(&self) -> Option<(&[u8], &[Checkpoint])> {
        let checkpoints = self.len().saturating_sub(1) / BLOCK_ENTRIES;
        let data_len = self
            .bytes
            .len()
            .checked_sub(checkpoints * CHECKPOINT_BYTES)?;
        let (data, index) = self.bytes.split_at(data_len);
        Some((data, index.as_chunks::<CHECKPOINT_BYTES>().0))
    }

    /// Decode the entries in order, validating block boundaries along the way.
    #[must_use]
    pub fn iter(&self) -> SpanIter<'_> {
        if let Some((data, checkpoints)) = self.parts() {
            SpanIter::new(data, self.count, checkpoints)
        } else {
            let mut malformed = SpanIter::new(&[], 0, &[]);
            malformed.failed = true;
            malformed
        }
    }

    /// Source span at a logical instruction. Before the first entry, use that
    /// entry's span. For equal PCs, the last matching entry wins.
    #[must_use]
    pub fn span_at(&self, pc: u32) -> Option<(u32, u32)> {
        let (data, checkpoints) = self.parts()?;
        let block = checkpoints.partition_point(|checkpoint| checkpoint_pc(checkpoint) <= pc);
        let start = if block == 0 {
            0
        } else {
            checkpoint_offset(&checkpoints[block - 1])?
        };
        let end = match checkpoints.get(block) {
            Some(checkpoint) => checkpoint_offset(checkpoint)?,
            None => data.len(),
        };
        let count = (self.len() - block * BLOCK_ENTRIES).min(BLOCK_ENTRIES) as u32;
        let mut entries = SpanIter::new(data.get(start..end)?, count, &[]);
        let first = entries.next()?;
        Some(
            entries
                .take_while(|entry| entry.pc <= pc)
                .last()
                .map_or(first.span, |entry| entry.span),
        )
    }

    /// Whether exactly `len()` entries decode and consume every byte.
    #[must_use]
    pub fn well_formed(&self) -> bool {
        let mut iter = self.iter();
        for _ in &mut iter {}
        iter.is_complete()
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
        let entries = entries.into_iter();
        let capacity = entries.size_hint().0;
        Self::encode(entries, capacity)
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
    block_entries: usize,
    data_len: usize,
    checkpoints: &'a [Checkpoint],
    pc: u32,
    start: u32,
    failed: bool,
}

impl<'a> SpanIter<'a> {
    fn new(bytes: &'a [u8], remaining: u32, checkpoints: &'a [Checkpoint]) -> Self {
        Self {
            bytes,
            remaining,
            block_entries: 0,
            data_len: bytes.len(),
            checkpoints,
            pc: 0,
            start: 0,
            failed: false,
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        !self.failed && self.remaining == 0 && self.bytes.is_empty() && self.checkpoints.is_empty()
    }

    fn begin_block(&mut self) -> Option<u32> {
        let (&checkpoint, rest) = self.checkpoints.split_first()?;
        let offset = self.data_len - self.bytes.len();
        if checkpoint_offset(&checkpoint)? != offset {
            return None;
        }
        self.checkpoints = rest;
        self.block_entries = 0;
        (self.pc, self.start) = (0, 0);
        Some(checkpoint_pc(&checkpoint))
    }

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
            let checkpoint = if self.block_entries == BLOCK_ENTRIES {
                Some(self.begin_block()?)
            } else {
                None
            };
            let pc = self.delta(self.pc)?;
            if checkpoint.is_some_and(|expected| expected != pc) {
                return None;
            }
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
                self.block_entries += 1;
                self.pc = entry.pc;
                self.start = entry.span.0;
                Some(entry)
            }
            None => {
                self.failed = true;
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
