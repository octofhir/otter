//! Borrowed match views and one owned capture arena per completed search.
//!
//! # Contents
//! - [`Match`] and [`Captures`] borrow interpreter, native or collected slots.
//! - [`MatchBatch`] owns flat rows and shares capture names once per search.
//! - [`MatchRecord`] owns decoded ranges for repeated single-result reads.
//! - [`NamedGroups`] selects duplicate names in deterministic source order.
//!
//! # Invariants
//! - Every offset counts UTF-16 code units, including one-byte subjects.
//! - Each batch row contains group zero followed by every capture's two slots.
//! - An unset encoded slot is the matcher's `NONE`; a capture needs both endpoints.
//! - Borrowed matcher views expire before the next search attempt. Owned rows
//!   never alias matcher scratch and may survive reentry or host allocation.
//! - Empty searches retain no name table; successful batches retain it once.
//!
//! # See also
//! - [`crate::Matches`] owns execution, scratch reuse and work accounting.

use crate::exec::native::NONE;
use core::ops::Range;
use smallvec::SmallVec;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Slots<'a> {
    Interpreted(&'a [Option<usize>]),
    Encoded(&'a [u64]),
}

impl Slots<'_> {
    #[inline]
    pub(crate) fn len(self) -> usize {
        match self {
            Self::Interpreted(slots) => slots.len(),
            Self::Encoded(slots) => slots.len(),
        }
    }

    #[inline]
    pub(crate) fn range(self) -> Range<usize> {
        match self {
            Self::Interpreted(slots) => slots[0].unwrap_or(0)..slots[1].unwrap_or(0),
            Self::Encoded(slots) => {
                let at = |index| {
                    let value = slots[index];
                    if value == NONE { 0 } else { value as usize }
                };
                at(0)..at(1)
            }
        }
    }
}

/// A match's borrowed capture ranges, excluding group zero.
#[derive(Debug, Clone, Copy)]
pub struct Captures<'a>(CaptureStorage<'a>);

#[derive(Debug, Clone, Copy)]
enum CaptureStorage<'a> {
    Slots(Slots<'a>),
    Ranges(&'a [Option<Range<usize>>]),
}

impl<'a> Captures<'a> {
    /// Number of capture groups, including groups that did not participate.
    #[must_use]
    #[inline]
    pub fn len(self) -> usize {
        match self.0 {
            CaptureStorage::Slots(slots) => slots.len() / 2,
            CaptureStorage::Ranges(ranges) => ranges.len(),
        }
    }

    /// Whether the pattern has no capture groups.
    #[must_use]
    #[inline]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Range of the zero-based capture, or `None` if absent or unmatched.
    #[must_use]
    #[inline]
    pub fn get(self, index: usize) -> Option<Range<usize>> {
        let slots = match self.0 {
            CaptureStorage::Ranges(ranges) => return ranges.get(index)?.clone(),
            CaptureStorage::Slots(slots) => slots,
        };
        let start = index.checked_mul(2)?;
        let end = start.checked_add(1)?;
        let (start, end) = match slots {
            Slots::Interpreted(slots) => (slots.get(start)?.as_ref()?, slots.get(end)?.as_ref()?),
            Slots::Encoded(slots) => {
                let start = *slots.get(start)?;
                let end = *slots.get(end)?;
                return (start != NONE && end != NONE).then_some(start as usize..end as usize);
            }
        };
        Some(*start..*end)
    }

    /// Capture ranges in numbered order, retaining unmatched entries.
    #[inline]
    pub fn iter(self) -> impl ExactSizeIterator<Item = Option<Range<usize>>> + Clone + 'a {
        (0..self.len()).map(move |index| self.get(index))
    }
}

/// One successful match borrowed from a search or an owned batch.
#[derive(Debug, Clone)]
pub struct Match<'a> {
    /// Overall match extent, group zero; it may be empty.
    pub range: Range<usize>,
    /// Capture ranges, with index zero naming group one.
    pub captures: Captures<'a>,
    group_names: &'a [String],
}

impl<'a> Match<'a> {
    #[inline]
    pub(crate) fn from_slots(names: &'a [String], slots: Slots<'a>) -> Self {
        let captures = match slots {
            Slots::Interpreted(slots) => Slots::Interpreted(&slots[2..]),
            Slots::Encoded(slots) => Slots::Encoded(&slots[2..]),
        };
        Self {
            range: slots.range(),
            captures: Captures(CaptureStorage::Slots(captures)),
            group_names: names,
        }
    }

    /// Access group zero or a one-based capture group.
    #[must_use]
    #[inline]
    pub fn group(&self, index: usize) -> Option<Range<usize>> {
        if index == 0 {
            Some(self.range.clone())
        } else {
            self.captures.get(index - 1)
        }
    }

    /// Select a named group's one-based index, preferring a matched alternative.
    #[must_use]
    #[inline]
    pub fn named_group_index(&self, name: &str) -> Option<usize> {
        named_index(self.group_names, self.captures, name)
    }

    /// Range selected by a capture name, or `None` for an unmatched name.
    #[must_use]
    #[inline]
    pub fn named_group(&self, name: &str) -> Option<Range<usize>> {
        self.named_group_index(name)
            .and_then(|index| self.group(index))
    }

    /// Iterate names once each in source order, preferring matched alternatives.
    #[must_use]
    #[inline]
    pub fn named_groups(&self) -> NamedGroups<'_> {
        NamedGroups {
            names: self.group_names,
            captures: self.captures,
            next: 0,
        }
    }
}

fn named_index(names: &[String], captures: Captures<'_>, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let mut first = None;
    for (index, candidate) in names.iter().enumerate() {
        if candidate == name {
            if captures.get(index).is_some() {
                return Some(index + 1);
            }
            first.get_or_insert(index + 1);
        }
    }
    first
}

/// One owned result with decoded ranges for repeated capture reads.
///
/// It is independent of matcher scratch and the subject; a match of up to
/// two groups allocates nothing, and only a pattern with named groups shares
/// its name table.
#[derive(Debug, Clone)]
pub struct MatchRecord {
    /// Overall match extent, group zero; it may be empty.
    pub range: Range<usize>,
    captures: SmallVec<[Option<Range<usize>>; 2]>,
    names: Option<Arc<[String]>>,
}

impl MatchRecord {
    #[inline]
    pub(crate) fn new(slots: Slots<'_>, names: &Arc<[String]>) -> Self {
        let captures = match slots {
            Slots::Encoded(slots) => slots[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    (pair[0] != NONE && pair[1] != NONE)
                        .then_some(pair[0] as usize..pair[1] as usize)
                })
                .collect(),
            Slots::Interpreted(slots) => slots[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| Some(pair[0]?..pair[1]?))
                .collect(),
        };
        Self {
            range: slots.range(),
            captures,
            names: names
                .iter()
                .any(|name| !name.is_empty())
                .then(|| Arc::clone(names)),
        }
    }

    /// Borrow numbered capture ranges, excluding group zero.
    #[must_use]
    #[inline]
    pub fn captures(&self) -> Captures<'_> {
        Captures(CaptureStorage::Ranges(&self.captures))
    }

    /// Borrow the same match view used by lending searches and batches.
    #[must_use]
    #[inline]
    pub fn as_match(&self) -> Match<'_> {
        Match {
            range: self.range.clone(),
            captures: self.captures(),
            group_names: self.names.as_deref().unwrap_or(&[]),
        }
    }
}

/// Deterministic, deduplicated named-capture iteration.
#[derive(Debug)]
pub struct NamedGroups<'a> {
    names: &'a [String],
    captures: Captures<'a>,
    next: usize,
}

impl<'a> Iterator for NamedGroups<'a> {
    type Item = (&'a str, Option<Range<usize>>);

    fn next(&mut self) -> Option<Self::Item> {
        while self.next < self.names.len() {
            let index = self.next;
            self.next += 1;
            let name = self.names[index].as_str();
            if name.is_empty() || self.names[..index].iter().any(|earlier| earlier == name) {
                continue;
            }
            let capture = named_index(self.names, self.captures, name)
                .and_then(|index| self.captures.get(index - 1));
            return Some((name, capture));
        }
        None
    }
}

/// Owned matches independent of the subject and reusable matcher scratch.
///
/// One inline arena covers small single matches; a global scan grows one flat
/// buffer instead of allocating a capture vector and cloning names per match.
#[derive(Debug, Clone)]
pub struct MatchBatch {
    slots: SmallVec<[u64; 2]>,
    stride: usize,
    names: Option<Arc<[String]>>,
}

impl Default for MatchBatch {
    fn default() -> Self {
        Self::new(2)
    }
}

impl MatchBatch {
    #[inline]
    pub(crate) fn new(capture_slots: usize) -> Self {
        debug_assert!(capture_slots >= 2 && capture_slots.is_multiple_of(2));
        Self {
            slots: SmallVec::new(),
            stride: capture_slots,
            names: None,
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, slots: Slots<'_>, names: &Arc<[String]>) {
        debug_assert_eq!(slots.len(), self.stride);
        if self.slots.is_empty() {
            self.names = Some(Arc::clone(names));
        }
        match slots {
            Slots::Encoded(slots) => self.slots.extend_from_slice(slots),
            Slots::Interpreted(slots) => self
                .slots
                .extend(slots.iter().map(|slot| slot.map_or(NONE, |at| at as u64))),
        }
    }

    /// Number of successful matches in the batch.
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.slots.len() / self.stride
    }

    /// Whether the search found no matches.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Match at a zero-based batch index.
    #[must_use]
    #[inline]
    pub fn get(&self, index: usize) -> Option<Match<'_>> {
        let start = index.checked_mul(self.stride)?;
        let end = start.checked_add(self.stride)?;
        self.slots
            .get(start..end)
            .map(|row| Match::from_slots(self.names.as_deref().unwrap_or(&[]), Slots::Encoded(row)))
    }

    /// First match, when present.
    #[must_use]
    #[inline]
    pub fn first(&self) -> Option<Match<'_>> {
        self.get(0)
    }

    /// Last match, when present.
    #[must_use]
    #[inline]
    pub fn last(&self) -> Option<Match<'_>> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }

    /// Borrow matches in their original search order.
    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Match<'_>> + DoubleEndedIterator + Clone {
        let names = self.names.as_deref().unwrap_or(&[]);
        self.slots
            .chunks_exact(self.stride)
            .map(move |row| Match::from_slots(names, Slots::Encoded(row)))
    }

    /// Retain at most `len` leading matches without changing their offsets.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        if len < self.len() {
            self.slots.truncate(len * self.stride);
        }
    }
}

#[cfg(test)]
mod tests;
