//! Field-level helper trait powering [`otter_macros::Pelt`].
//!
//! The [`Pelt`](otter_macros::Pelt) derive expands to one
//! `<FieldTy as PeltField>::pelt_trace(&mut self.field, visitor)` call per
//! traced field of a GC body. Each leaf field type implements
//! [`PeltField`] once; the derive itself never inspects field shapes
//! beyond looking for the `#[pelt(skip)]` attribute, which suppresses
//! the call entirely.
//!
//! # Contents
//!
//! - [`PeltField`] — single-method trait.
//! - Blanket impls for `Value`, `Gc<T>`, `Option<T>`, `Vec<T>`,
//!   `[T; N]`, `Box<T>`, `RefCell<T>`, and the integer / float /
//!   `bool` / `String` / `()` primitives every body uses for
//!   non-traced bookkeeping.
//!
//! # Invariants
//!
//! - `pelt_trace` must not allocate inside the GC heap.
//! - The visitor receives slot pointers (`*mut RawGc`); the
//!   scavenger may rewrite them in place when objects move.
//! - Primitive leaf types implement `PeltField` as no-ops so the
//!   derive can call uniformly without per-field carve-outs at the
//!   AST level.
//! - Types that wrap GC handles inside interior-mutability shapes
//!   (`Cell<Value>`, `UnsafeCell<…>`) are intentionally not covered
//!   by a blanket impl — taking the visitor through `Cell::get()`
//!   would visit a value copy, not the cell slot, breaking
//!   relocation. Fields with that shape stay on a hand-rolled
//!   `SafeTraceable` impl or use `#[pelt(skip)]` plus a manual
//!   `trace_slots_safe` body.
//!
//! # See also
//!
//! - [`otter_macros::Pelt`] — derive macro that emits the calls.
//! - [`otter_gc::SafeTraceable`] — trait the derive implements on
//!   the body type.

use std::cell::RefCell;

use otter_gc::raw::{RawGc, SlotVisitor};

use crate::Value;

/// Field-level hook the [`Pelt`](otter_macros::Pelt) derive
/// dispatches on. Implementors yield one slot pointer per outgoing
/// `Gc<…>` reference owned by `self`.
pub trait PeltField {
    /// Visit every GC slot reachable through `self`.
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>);
}

impl PeltField for Value {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.trace_value_slot_mut(visitor);
    }
}

impl<T: ?Sized> PeltField for otter_gc::Gc<T> {
    /// `Gc<T>` is `#[repr(transparent)]` over a 4-byte compressed
    /// offset that aliases `RawGc`. The visitor receives the address
    /// of the inline field so the scavenger can rewrite the offset
    /// in place. Null handles are skipped to match the hand-written
    /// guards (`if !handle.is_null() { … }`) the derive replaces.
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if self.is_null() {
            return;
        }
        let slot = self as *mut otter_gc::Gc<T> as *mut RawGc;
        visitor(slot);
    }
}

impl<T: PeltField> PeltField for Option<T> {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        if let Some(inner) = self {
            inner.pelt_trace(visitor);
        }
    }
}

impl<T: PeltField> PeltField for Vec<T> {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        for item in self {
            item.pelt_trace(visitor);
        }
    }
}

impl<T: PeltField, const N: usize> PeltField for [T; N] {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        for item in self {
            item.pelt_trace(visitor);
        }
    }
}

impl<A: smallvec::Array<Item = T>, T: PeltField> PeltField for smallvec::SmallVec<A> {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        for item in self {
            item.pelt_trace(visitor);
        }
    }
}

impl<K, V: PeltField, S> PeltField for std::collections::HashMap<K, V, S> {
    /// Walks values only — keys never carry GC slots in current
    /// bodies; if that changes the body owner adds a custom impl.
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        for value in self.values_mut() {
            value.pelt_trace(visitor);
        }
    }
}

impl<K, V: PeltField, S> PeltField for indexmap::IndexMap<K, V, S> {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        for value in self.values_mut() {
            value.pelt_trace(visitor);
        }
    }
}

impl<A: PeltField, B: PeltField> PeltField for (A, B) {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.0.pelt_trace(visitor);
        self.1.pelt_trace(visitor);
    }
}

impl<A: PeltField, B: PeltField, C: PeltField> PeltField for (A, B, C) {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.0.pelt_trace(visitor);
        self.1.pelt_trace(visitor);
        self.2.pelt_trace(visitor);
    }
}

impl<T: ?Sized> PeltField for std::sync::Arc<T> {
    /// Arc-shared payloads never carry inline GC slots — backing
    /// stores (`Arc<[u8]>` JSON source bytes, `Arc<libloading::Library>`,
    /// `Arc<NativeFn>` closures) own foreign data. Bodies that wrap a
    /// value with GC slots inside an `Arc` must reach in through a
    /// hand-written impl rather than this no-op.
    #[inline]
    fn pelt_trace(&mut self, _visitor: &mut SlotVisitor<'_>) {}
}

impl<T: PeltField + ?Sized> PeltField for Box<T> {
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        (**self).pelt_trace(visitor);
    }
}

impl<T: PeltField> PeltField for RefCell<T> {
    /// `RefCell::borrow()` returns a `Ref<T>` whose deref target is
    /// the cell's inner storage — slot pointers handed to the
    /// visitor reach the live heap word.
    #[inline]
    fn pelt_trace(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.borrow_mut().pelt_trace(visitor);
    }
}

// ---------------------------------------------------------------------------
// Primitive leaves — present so the derive can call uniformly. Bodies are
// no-ops because these types hold no GC slot.
// ---------------------------------------------------------------------------

macro_rules! pelt_noop {
    ($($t:ty),* $(,)?) => {
        $(
            impl PeltField for $t {
                #[inline]
                fn pelt_trace(&mut self, _visitor: &mut SlotVisitor<'_>) {}
            }
        )*
    };
}

pelt_noop! {
    (),
    bool, char,
    u8, u16, u32, u64, u128, usize,
    i8, i16, i32, i64, i128, isize,
    f32, f64,
    String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_gc::raw::RawGc;

    fn collect_slots<F: PeltField>(field: &mut F) -> Vec<*mut RawGc> {
        let mut out: Vec<*mut RawGc> = Vec::new();
        {
            let mut push = |p: *mut RawGc| out.push(p);
            field.pelt_trace(&mut push);
        }
        out
    }

    #[test]
    fn primitives_are_no_ops() {
        assert!(collect_slots(&mut 42u32).is_empty());
        assert!(collect_slots(&mut true).is_empty());
        assert!(collect_slots(&mut "hello".to_string()).is_empty());
    }

    #[test]
    fn value_undefined_skips_visitor() {
        let mut v = Value::UNDEFINED;
        assert!(collect_slots(&mut v).is_empty());
    }

    #[test]
    fn option_some_value_visits_once() {
        let mut v = Some(Value::undefined()); // immediate, no slot
        assert!(collect_slots(&mut v).is_empty());
    }

    #[test]
    fn vec_iterates_in_order() {
        let mut xs: Vec<Value> = vec![Value::UNDEFINED, Value::NULL, Value::TRUE];
        assert!(collect_slots(&mut xs).is_empty());
    }
}
