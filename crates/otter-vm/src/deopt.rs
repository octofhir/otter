//! Exact-PC deopt frame-state and safepoint stack-map ABI.
//!
//! This module defines the two records that let a moving collector and an
//! optimizing tier coexist. The optimizing tier *populates* them when it
//! compiles a function; this module only fixes their shape and the
//! reconstitution rules, so the contract is final before any code bakes
//! against it.
//!
//! 1. **Frame-state table** ([`DeoptTable`]) — keyed by interpreter byte-PC.
//!    For each deopt point it records, per interpreter virtual register, where
//!    the value lives ([`DeoptLocation`]) and how to turn its raw bits back
//!    into a full tagged [`Value`] ([`DeoptRepr`]). A guard failure or lazy
//!    deopt reconstructs the exact interpreter frame at the right PC by walking
//!    the matching [`FrameState`].
//!
//! 2. **Safepoint stack maps** ([`SafepointTable`]) — one [`StackMap`] per
//!    GC-safe point (every call and allocation site), marking which compiled
//!    slots hold a tagged, rootable pointer. The moving collector consults the
//!    map for the active safepoint to find and relocate the roots an optimized
//!    frame holds, without conservatively scanning the stack.
//!
//! # Reconstitution
//!
//! A register held unboxed in compiled code must be re-tagged on the way out.
//! [`DeoptRepr::reconstitute`] is the single source of truth: an `Int32` slot
//! re-tags through [`Value::number_i32`], a `Float64` slot re-boxes through
//! [`Value::number_f64`] (both apply the frozen value encoding), and a
//! `Tagged` slot is already a full `Value`.
//!
//! # Invariants
//!
//! - A [`DeoptTable`] / [`SafepointTable`] is sorted by byte-PC; lookups are an
//!   exact-match binary search. A point with no entry is not a valid deopt /
//!   safepoint and lookups return `None`.
//! - A [`FrameState`] carries one [`DeoptSlot`] per interpreter virtual
//!   register the frame defines, in register-index order, matching the windowed
//!   register numbering the frame ABI fixes.
//! - A [`StackMap`] indexes the same compiled slots the frame state locates;
//!   bit `i` set means slot `i` holds a tagged pointer the collector relocates.

use crate::Value;

/// How a deopt slot's raw bits reconstitute into a full tagged [`Value`].
///
/// The optimizing tier may keep a value unboxed across a region (an int in a
/// general register, a double in an FP register); the deopt record names the
/// representation so the exit re-tags it into the boxed `Value` the
/// interpreter frame expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptRepr {
    /// Already a full 8-byte tagged `Value`; the raw bits are the value.
    Tagged,
    /// An unboxed `i32` in the low 32 bits; re-tag to a number `Value`.
    Int32,
    /// An unboxed `f64` bit pattern; re-box to a number `Value`.
    Float64,
}

impl DeoptRepr {
    /// Reconstitute the full tagged [`Value`] from a slot's raw 64-bit payload.
    /// `raw` is the machine word read from the slot's [`DeoptLocation`].
    #[must_use]
    pub fn reconstitute(self, raw: u64) -> Value {
        match self {
            DeoptRepr::Tagged => Value::from_bits(raw),
            DeoptRepr::Int32 => Value::number_i32(raw as u32 as i32),
            DeoptRepr::Float64 => Value::number_f64(f64::from_bits(raw)),
        }
    }
}

/// Where a value lives at a deopt point, relative to the optimized frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeoptLocation {
    /// A machine register, by the optimizing tier's register id.
    Register(u16),
    /// A spill stack slot, by signed byte offset from the frame pointer.
    StackSlot(i32),
    /// A compile-time constant, by index into the function's constant pool.
    Constant(u32),
}

/// One interpreter virtual register at a deopt point: where it lives and how to
/// turn it back into a tagged [`Value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeoptSlot {
    /// Where the value lives in the optimized frame.
    pub location: DeoptLocation,
    /// How to reconstitute the boxed `Value` from the raw bits at `location`.
    pub repr: DeoptRepr,
}

/// The interpreter-frame reconstruction record for one deopt point.
///
/// Reconstructing the frame means materializing each [`DeoptSlot`] (read the
/// raw bits at its location, [`DeoptRepr::reconstitute`]) into the interpreter
/// register of the same index, then resuming the interpreter at `byte_pc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameState {
    /// Interpreter byte-PC to resume at after the exit.
    pub byte_pc: u32,
    /// One slot per interpreter virtual register the frame defines, in
    /// register-index order.
    pub slots: Box<[DeoptSlot]>,
}

/// Per-compiled-function deopt table, looked up by interpreter byte-PC.
///
/// Sorted by `byte_pc`; [`Self::lookup`] is an exact-match binary search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptTable {
    entries: Vec<FrameState>,
}

impl DeoptTable {
    /// Build a table from frame states. They are sorted by `byte_pc`; two
    /// states at the same PC is a builder error (a deopt point is unique).
    #[must_use]
    pub fn from_states(mut states: Vec<FrameState>) -> Self {
        states.sort_by_key(|s| s.byte_pc);
        debug_assert!(
            states.windows(2).all(|w| w[0].byte_pc != w[1].byte_pc),
            "two frame states at the same byte_pc"
        );
        Self { entries: states }
    }

    /// The frame state for `byte_pc`, or `None` when the PC is not a deopt
    /// point.
    #[must_use]
    pub fn lookup(&self, byte_pc: u32) -> Option<&FrameState> {
        let i = self
            .entries
            .binary_search_by_key(&byte_pc, |s| s.byte_pc)
            .ok()?;
        Some(&self.entries[i])
    }

    /// Number of recorded deopt points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table records no deopt points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A compact bitset over a safepoint's compiled slots: bit `i` set means slot
/// `i` holds a tagged pointer the moving collector must find and relocate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMap {
    words: Box<[u64]>,
}

impl StackMap {
    /// Build a stack map sized for `slot_count` slots, with the slots in
    /// `tagged` marked. Out-of-range indices are ignored.
    #[must_use]
    pub fn from_tagged_slots(slot_count: usize, tagged: impl IntoIterator<Item = usize>) -> Self {
        let words = slot_count.div_ceil(64);
        let mut bits = vec![0u64; words].into_boxed_slice();
        for slot in tagged {
            if slot < slot_count {
                bits[slot / 64] |= 1u64 << (slot % 64);
            }
        }
        Self { words: bits }
    }

    /// Whether slot `i` holds a tagged root.
    #[must_use]
    pub fn is_tagged(&self, i: usize) -> bool {
        let word = i / 64;
        word < self.words.len() && self.words[word] & (1u64 << (i % 64)) != 0
    }

    /// Visit each tagged slot index in ascending order.
    pub fn for_each_tagged(&self, mut f: impl FnMut(usize)) {
        for (w, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                f(w * 64 + bit);
                bits &= bits - 1;
            }
        }
    }
}

/// One GC-safe point: the PC it covers and the tagged-slot map at that point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Safepoint {
    /// Interpreter byte-PC of the safe point (a call or allocation site).
    pub byte_pc: u32,
    /// Which compiled slots hold tagged roots at this point.
    pub tagged: StackMap,
}

/// Per-compiled-function safepoint table, looked up by byte-PC.
///
/// Sorted by `byte_pc`; [`Self::lookup`] is an exact-match binary search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SafepointTable {
    entries: Vec<Safepoint>,
}

impl SafepointTable {
    /// Build a table from safepoints. They are sorted by `byte_pc`.
    #[must_use]
    pub fn from_safepoints(mut points: Vec<Safepoint>) -> Self {
        points.sort_by_key(|p| p.byte_pc);
        debug_assert!(
            points.windows(2).all(|w| w[0].byte_pc != w[1].byte_pc),
            "two safepoints at the same byte_pc"
        );
        Self { entries: points }
    }

    /// The stack map for `byte_pc`, or `None` when the PC is not a safe point.
    #[must_use]
    pub fn lookup(&self, byte_pc: u32) -> Option<&StackMap> {
        let i = self
            .entries
            .binary_search_by_key(&byte_pc, |p| p.byte_pc)
            .ok()?;
        Some(&self.entries[i].tagged)
    }

    /// Number of recorded safe points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table records no safe points.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconstitute_matches_the_value_encoding() {
        assert_eq!(DeoptRepr::Int32.reconstitute(5), Value::number_i32(5));
        assert_eq!(
            DeoptRepr::Int32.reconstitute(u32::MAX as u64),
            Value::number_i32(-1)
        );
        assert_eq!(
            DeoptRepr::Float64.reconstitute(3.5f64.to_bits()),
            Value::number_f64(3.5)
        );
        let v = Value::number_i32(42);
        assert_eq!(DeoptRepr::Tagged.reconstitute(v.to_bits()), v);
    }

    #[test]
    fn deopt_table_is_exact_pc() {
        let slot = DeoptSlot {
            location: DeoptLocation::Register(3),
            repr: DeoptRepr::Int32,
        };
        let table = DeoptTable::from_states(vec![
            FrameState {
                byte_pc: 40,
                slots: vec![slot].into(),
            },
            FrameState {
                byte_pc: 8,
                slots: vec![slot].into(),
            },
        ]);
        assert_eq!(table.len(), 2);
        assert!(table.lookup(8).is_some());
        assert_eq!(table.lookup(40).unwrap().slots[0].location, slot.location);
        // A non-deopt PC has no entry — exact match, no nearest-PC fallback.
        assert!(table.lookup(20).is_none());
    }

    #[test]
    fn stack_map_marks_only_tagged_slots() {
        let map = StackMap::from_tagged_slots(70, [0usize, 5, 64, 200]);
        assert!(map.is_tagged(0));
        assert!(map.is_tagged(5));
        assert!(map.is_tagged(64));
        assert!(!map.is_tagged(1));
        assert!(!map.is_tagged(69));
        // 200 was out of range and ignored.
        assert!(!map.is_tagged(200));
        let mut seen = Vec::new();
        map.for_each_tagged(|i| seen.push(i));
        assert_eq!(seen, vec![0, 5, 64]);
    }

    #[test]
    fn safepoint_table_lookup() {
        let table = SafepointTable::from_safepoints(vec![
            Safepoint {
                byte_pc: 16,
                tagged: StackMap::from_tagged_slots(4, [1usize]),
            },
            Safepoint {
                byte_pc: 4,
                tagged: StackMap::from_tagged_slots(4, [0usize]),
            },
        ]);
        assert_eq!(table.len(), 2);
        assert!(table.lookup(4).unwrap().is_tagged(0));
        assert!(table.lookup(16).unwrap().is_tagged(1));
        assert!(table.lookup(9).is_none());
    }
}
