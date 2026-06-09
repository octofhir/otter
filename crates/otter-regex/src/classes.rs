//! Code-point sets and the UnicodeSets (`v`-flag) set algebra.
//!
//! A [`CodePointSet`] is a sorted, disjoint list of inclusive code-point
//! ranges — the representation for character classes, `\p{...}` properties, and
//! the `\d \w \s` (and negated) escapes. Under the `v` flag, classes form an
//! algebra: union, intersection (`&&`), and difference (`--`), plus nesting and
//! `\q{...}` string alternatives.
//!
//! # Contents
//! - [`CodePointSet`] — sorted disjoint inclusive code-point ranges.
//! - [`ClassSet`] — a `v`-mode class that may also carry string alternatives.
//!
//! # Invariants
//! - Ranges in a [`CodePointSet`] are sorted ascending and pairwise disjoint;
//!   adjacent ranges are merged on insertion.
//! - All code points are in `0..=0x10FFFF` (scalar values and lone surrogates).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-classsetexpression> (§22.2.1 `v`-flag classes)

use core::ops::RangeInclusive;

/// The largest representable code point.
const MAX_CP: u32 = 0x10FFFF;

/// A sorted, disjoint set of inclusive code-point ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CodePointSet {
    ranges: Vec<RangeInclusive<u32>>,
}

impl CodePointSet {
    /// An empty set.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Build from an iterator of inclusive ranges, normalising once. Cheap when
    /// the input is already sorted and disjoint (e.g. a UCD property table).
    pub(crate) fn from_ranges(iter: impl Iterator<Item = RangeInclusive<u32>>) -> Self {
        let mut s = Self {
            ranges: iter.collect(),
        };
        s.normalize();
        s
    }

    /// The sorted, disjoint ranges.
    #[must_use]
    pub(crate) fn ranges(&self) -> &[RangeInclusive<u32>] {
        &self.ranges
    }

    /// Whether the set has no members.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Add a single code point.
    pub(crate) fn insert(&mut self, cp: u32) {
        self.insert_range(cp, cp);
    }

    /// Add an inclusive range, keeping the set sorted and disjoint.
    pub(crate) fn insert_range(&mut self, lo: u32, hi: u32) {
        if lo > hi {
            return;
        }
        self.ranges.push(lo..=hi);
        self.normalize();
    }

    /// Re-sort and merge overlapping/adjacent ranges.
    fn normalize(&mut self) {
        if self.ranges.len() < 2 {
            return;
        }
        self.ranges.sort_by_key(|r| *r.start());
        let mut merged: Vec<RangeInclusive<u32>> = Vec::with_capacity(self.ranges.len());
        for r in self.ranges.drain(..) {
            match merged.last_mut() {
                Some(last) if *r.start() <= last.end().saturating_add(1) => {
                    if r.end() > last.end() {
                        *last = *last.start()..=*r.end();
                    }
                }
                _ => merged.push(r),
            }
        }
        self.ranges = merged;
    }

    /// Whether `cp` is a member (binary search over disjoint ranges).
    #[must_use]
    pub(crate) fn contains(&self, cp: u32) -> bool {
        self.ranges
            .binary_search_by(|r| {
                if cp < *r.start() {
                    core::cmp::Ordering::Greater
                } else if cp > *r.end() {
                    core::cmp::Ordering::Less
                } else {
                    core::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// Set union (`A` or `B`).
    #[must_use]
    pub(crate) fn union(&self, other: &CodePointSet) -> CodePointSet {
        let mut out = self.clone();
        for r in &other.ranges {
            out.ranges.push(r.clone());
        }
        out.normalize();
        out
    }

    /// Add every member of `other` into `self`.
    pub(crate) fn union_with(&mut self, other: &CodePointSet) {
        for r in &other.ranges {
            self.ranges.push(r.clone());
        }
        self.normalize();
    }

    /// Set complement within `0..=0x10FFFF`.
    #[must_use]
    pub(crate) fn negate(&self) -> CodePointSet {
        let mut out = Vec::new();
        let mut next: u32 = 0;
        for r in &self.ranges {
            let lo = *r.start();
            let hi = *r.end();
            if lo > next {
                out.push(next..=lo - 1);
            }
            if hi >= MAX_CP {
                return CodePointSet { ranges: out };
            }
            next = hi + 1;
        }
        if next <= MAX_CP {
            out.push(next..=MAX_CP);
        }
        CodePointSet { ranges: out }
    }

    /// Set intersection (`A && B`) via a two-pointer sweep.
    #[must_use]
    pub(crate) fn intersection(&self, other: &CodePointSet) -> CodePointSet {
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < self.ranges.len() && j < other.ranges.len() {
            let a = &self.ranges[i];
            let b = &other.ranges[j];
            let lo = *a.start().max(b.start());
            let hi = *a.end().min(b.end());
            if lo <= hi {
                out.push(lo..=hi);
            }
            if a.end() < b.end() {
                i += 1;
            } else {
                j += 1;
            }
        }
        CodePointSet { ranges: out }
    }

    /// Set difference (`A -- B`).
    #[must_use]
    pub(crate) fn difference(&self, other: &CodePointSet) -> CodePointSet {
        self.intersection(&other.negate())
    }
}

/// A `v`-mode class: a code-point set plus optional string alternatives.
///
/// `\q{ab|c}` and string-valued properties (`\p{Basic_Emoji}`) contribute
/// multi-code-point alternatives that a plain [`CodePointSet`] cannot express.
/// In non-`v` modes `strings` is always empty.
#[derive(Debug, Clone, Default)]
pub(crate) struct ClassSet {
    /// Single-code-point members.
    pub(crate) code_points: CodePointSet,
    /// Multi-code-point string alternatives, longest-first for match priority.
    pub(crate) strings: Vec<Vec<u32>>,
}

impl ClassSet {
    /// Wrap a code-point set with no string alternatives.
    #[must_use]
    pub(crate) fn from_code_points(code_points: CodePointSet) -> Self {
        Self {
            code_points,
            strings: Vec::new(),
        }
    }

    /// `true` when this set has any multi-code-point string alternative
    /// (`MayContainStrings`, §22.2.1.4). A negated `v`-mode class
    /// `[^...]` is a syntax error when this holds.
    #[must_use]
    pub(crate) fn may_contain_strings(&self) -> bool {
        !self.strings.is_empty()
    }

    /// Add one alternative. A single code point joins `code_points`; a
    /// multi-code-point (or empty) alternative joins `strings`, kept
    /// deduplicated.
    pub(crate) fn add_alternative(&mut self, alt: Vec<u32>) {
        if alt.len() == 1 {
            self.code_points.insert(alt[0]);
        } else if !self.strings.contains(&alt) {
            self.strings.push(alt);
        }
    }

    /// `A ∪ B` (`ClassUnion`).
    pub(crate) fn union_with(&mut self, other: &ClassSet) {
        self.code_points.union_with(&other.code_points);
        for s in &other.strings {
            if !self.strings.contains(s) {
                self.strings.push(s.clone());
            }
        }
    }

    /// `A ∩ B` (`ClassIntersection`, `&&`). A string survives only when
    /// it is present in both operands (single-character members are kept
    /// in `code_points`, so multi-character strings intersect by value).
    #[must_use]
    pub(crate) fn intersection(&self, other: &ClassSet) -> ClassSet {
        ClassSet {
            code_points: self.code_points.intersection(&other.code_points),
            strings: self
                .strings
                .iter()
                .filter(|s| other.strings.contains(*s))
                .cloned()
                .collect(),
        }
    }

    /// `A -- B` (`ClassSetDifference`). Removes B's code points and any
    /// string alternative B also contains.
    #[must_use]
    pub(crate) fn difference(&self, other: &ClassSet) -> ClassSet {
        ClassSet {
            code_points: self.code_points.difference(&other.code_points),
            strings: self
                .strings
                .iter()
                .filter(|s| !other.strings.contains(*s))
                .cloned()
                .collect(),
        }
    }

    /// Negate the code-point membership (`[^...]`). The caller must have
    /// verified [`Self::may_contain_strings`] is false — a negated class
    /// with strings is a syntax error.
    #[must_use]
    pub(crate) fn negate_code_points(&self) -> ClassSet {
        ClassSet {
            code_points: self.code_points.negate(),
            strings: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_merges_adjacent_and_overlapping() {
        let mut s = CodePointSet::new();
        s.insert_range(b'a' as u32, b'c' as u32);
        s.insert_range(b'd' as u32, b'f' as u32); // adjacent -> merge
        s.insert_range(b'b' as u32, b'b' as u32); // inside -> no change
        assert_eq!(s.ranges(), &[(b'a' as u32)..=(b'f' as u32)]);
    }

    #[test]
    fn membership() {
        let mut s = CodePointSet::new();
        s.insert_range(b'0' as u32, b'9' as u32);
        assert!(s.contains(b'5' as u32));
        assert!(!s.contains(b'a' as u32));
    }

    #[test]
    fn negate_round_trips() {
        let mut s = CodePointSet::new();
        s.insert_range(b'a' as u32, b'z' as u32);
        let n = s.negate();
        assert!(!n.contains(b'a' as u32));
        assert!(n.contains(b'A' as u32));
        assert_eq!(n.negate(), s);
    }

    #[test]
    fn intersection_and_difference() {
        let mut a = CodePointSet::new();
        a.insert_range(b'a' as u32, b'm' as u32);
        let mut b = CodePointSet::new();
        b.insert_range(b'h' as u32, b'z' as u32);
        let i = a.intersection(&b);
        assert_eq!(i.ranges(), &[(b'h' as u32)..=(b'm' as u32)]);
        let d = a.difference(&b);
        assert_eq!(d.ranges(), &[(b'a' as u32)..=(b'g' as u32)]);
    }
}
