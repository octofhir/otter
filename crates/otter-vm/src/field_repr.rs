//! Monotone per-slot field-representation tracking.
//!
//! A property slot whose stored value has stayed an `int32` (or a `double`)
//! across every write can be held unboxed in a register by the optimizing tier
//! and read without a tag check. This module records, per `(shape, slot)`, the
//! widest representation ever stored, under a lattice that only ever widens —
//! `Int32` ⊑ `Float64` ⊑ `Tagged` — so a slot can never be mis-speculated as
//! narrower than a value it has actually held.
//!
//! The optimizing tier reads a slot's representation at compile time to bake an
//! unboxed load plus a deopt-on-widen guard; a later store that widens the slot
//! both updates the record and trips that guard (the frame-state record drives
//! the exit). Recording is advisory: an unseen slot reads as unknown and is
//! lowered generically, and losing a record is always sound — only less fast.
//!
//! # Invariants
//!
//! - **Monotone.** A slot's representation only ever widens. [`FieldRepr::join`]
//!   is the widening; [`FieldReprTable::record`] applies it and never narrows.
//! - **Advisory.** A slot never recorded reads as `None`; the optimizing tier
//!   treats that as unknown.

use rustc_hash::FxHashMap;

use crate::Value;

/// The representation a field slot's value can be held in, ordered by width:
/// `Int32` is narrowest, `Tagged` is widest (a fully boxed `Value`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldRepr {
    /// Every stored value has been an `int32`; holdable as an unboxed integer.
    Int32,
    /// Stored values have been numbers but not all `int32`; holdable as an
    /// unboxed `f64`.
    Float64,
    /// Stored values have included a non-number; must be a boxed `Value`.
    Tagged,
}

impl FieldRepr {
    /// Lattice rank — higher is wider. `Int32 < Float64 < Tagged`.
    #[must_use]
    const fn rank(self) -> u8 {
        match self {
            FieldRepr::Int32 => 0,
            FieldRepr::Float64 => 1,
            FieldRepr::Tagged => 2,
        }
    }

    /// The widening join: the wider of the two representations. Monotone — the
    /// result is never narrower than either input.
    #[must_use]
    pub fn join(self, other: FieldRepr) -> FieldRepr {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    /// Classify a stored value into its narrowest representation: an `int32`
    /// number is `Int32`, any other number is `Float64`, everything else is
    /// `Tagged`.
    #[must_use]
    pub fn observe(value: Value) -> FieldRepr {
        if value.is_int32() {
            FieldRepr::Int32
        } else if value.is_number() {
            FieldRepr::Float64
        } else {
            FieldRepr::Tagged
        }
    }
}

/// Per-`(shape, slot)` widest-representation record.
///
/// Keyed by the raw shape id (`ShapeId::raw`) and the string-keyed slot offset.
/// Lives on the interpreter (per-isolate), like the other JIT feedback tables;
/// it is never a process global.
#[derive(Debug, Default)]
pub struct FieldReprTable {
    reprs: FxHashMap<(u64, u16), FieldRepr>,
}

impl FieldReprTable {
    /// Empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a stored `value` into the `(shape, slot)` record, widening it.
    /// Returns `true` when the recorded representation widened — the event the
    /// optimizing tier's deopt-on-widen guard responds to (compiled code that
    /// assumed the narrower representation must exit).
    pub fn record(&mut self, shape_id: u64, slot: u16, value: Value) -> bool {
        let observed = FieldRepr::observe(value);
        match self.reprs.get_mut(&(shape_id, slot)) {
            Some(current) => {
                let widened = current.join(observed);
                if widened != *current {
                    *current = widened;
                    true
                } else {
                    false
                }
            }
            None => {
                self.reprs.insert((shape_id, slot), observed);
                // A first observation establishes the slot's representation; it
                // is not a widening of an existing assumption.
                false
            }
        }
    }

    /// The recorded representation for `(shape, slot)`, or `None` when the slot
    /// has never been observed.
    #[must_use]
    pub fn get(&self, shape_id: u64, slot: u16) -> Option<FieldRepr> {
        self.reprs.get(&(shape_id, slot)).copied()
    }

    /// Number of recorded slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.reprs.len()
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reprs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_only_widens() {
        use FieldRepr::*;
        assert_eq!(Int32.join(Int32), Int32);
        assert_eq!(Int32.join(Float64), Float64);
        assert_eq!(Float64.join(Int32), Float64);
        assert_eq!(Float64.join(Tagged), Tagged);
        assert_eq!(Tagged.join(Int32), Tagged);
        // Symmetric and idempotent.
        assert_eq!(Tagged.join(Tagged), Tagged);
    }

    #[test]
    fn observe_classifies_by_width() {
        assert_eq!(FieldRepr::observe(Value::number_i32(7)), FieldRepr::Int32);
        assert_eq!(
            FieldRepr::observe(Value::number_f64(1.5)),
            FieldRepr::Float64
        );
        assert_eq!(FieldRepr::observe(Value::undefined()), FieldRepr::Tagged);
        assert_eq!(FieldRepr::observe(Value::null()), FieldRepr::Tagged);
    }

    #[test]
    fn record_is_monotone_and_signals_widening() {
        let mut table = FieldReprTable::new();
        // First observation: establishes Int32, not a widening.
        assert!(!table.record(1, 0, Value::number_i32(3)));
        assert_eq!(table.get(1, 0), Some(FieldRepr::Int32));
        // Another int32: no change.
        assert!(!table.record(1, 0, Value::number_i32(9)));
        assert_eq!(table.get(1, 0), Some(FieldRepr::Int32));
        // A double widens Int32 -> Float64: signals.
        assert!(table.record(1, 0, Value::number_f64(2.5)));
        assert_eq!(table.get(1, 0), Some(FieldRepr::Float64));
        // An int32 after a double does NOT narrow back.
        assert!(!table.record(1, 0, Value::number_i32(4)));
        assert_eq!(table.get(1, 0), Some(FieldRepr::Float64));
        // A non-number widens to Tagged: signals.
        assert!(table.record(1, 0, Value::undefined()));
        assert_eq!(table.get(1, 0), Some(FieldRepr::Tagged));

        // A different (shape, slot) is independent.
        assert!(!table.record(2, 0, Value::number_i32(1)));
        assert_eq!(table.get(2, 0), Some(FieldRepr::Int32));
        assert_eq!(table.get(1, 5), None);
    }
}
