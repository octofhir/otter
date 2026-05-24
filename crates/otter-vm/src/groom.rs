//! Field-level helper trait powering [`otter_macros::Groom`].
//!
//! The [`Groom`](otter_macros::Groom) derive expands to one
//! `<FieldTy as GroomField>::groom(&mut self.field)` call per non-skipped
//! field of a GC body. Each leaf field type implements [`GroomField`]
//! once; the derive itself never inspects field shapes beyond looking
//! for the `#[groom(skip)]` attribute, which suppresses the call
//! entirely.
//!
//! # Contents
//!
//! - [`GroomField`] — single-method trait.
//! - Blanket impls for `Option<T>`, `Vec<T>`, `[T; N]`, `Box<T>`,
//!   `RefCell<T>`, `SmallVec<…>`, tuples, plus the primitive leaves
//!   every body uses for bookkeeping (no-ops).
//!
//! # Invariants
//!
//! - `groom` runs once per dead body, before `Drop`. The body's
//!   storage is still valid for the duration of the call.
//! - `groom` must not allocate inside the GC heap, must not run
//!   user JavaScript, and must not re-enter the same heap.
//! - Primitive leaf types implement `GroomField` as no-ops so the
//!   derive can call uniformly without per-field carve-outs at the
//!   AST level.
//!
//! # See also
//!
//! - [`otter_macros::Groom`] — derive macro that emits the calls.
//! - [`otter_gc::SafeFinalize`] — trait the derive implements on
//!   the body type.

use std::cell::RefCell;

/// Field-level hook the [`Groom`](otter_macros::Groom) derive
/// dispatches on. Implementors release any sweep-time resource
/// associated with `self` (decrement a host registry, null out an
/// external counter, …). Default leaf impls are no-ops; bodies
/// that need real cleanup add a custom impl or use
/// `#[groom(via = …)]` to point at one.
pub trait GroomField {
    /// Sweep-time finalize hook. Default is a no-op.
    fn groom(&mut self);
}

impl<T: GroomField> GroomField for Option<T> {
    #[inline]
    fn groom(&mut self) {
        if let Some(inner) = self.as_mut() {
            inner.groom();
        }
    }
}

impl<T: GroomField> GroomField for Vec<T> {
    #[inline]
    fn groom(&mut self) {
        for item in self.iter_mut() {
            item.groom();
        }
    }
}

impl<T: GroomField, const N: usize> GroomField for [T; N] {
    #[inline]
    fn groom(&mut self) {
        for item in self.iter_mut() {
            item.groom();
        }
    }
}

impl<A: smallvec::Array<Item = T>, T: GroomField> GroomField for smallvec::SmallVec<A> {
    #[inline]
    fn groom(&mut self) {
        for item in self.iter_mut() {
            item.groom();
        }
    }
}

impl<T: GroomField + ?Sized> GroomField for Box<T> {
    #[inline]
    fn groom(&mut self) {
        (**self).groom();
    }
}

impl<T: GroomField> GroomField for RefCell<T> {
    #[inline]
    fn groom(&mut self) {
        self.get_mut().groom();
    }
}

impl<A: GroomField, B: GroomField> GroomField for (A, B) {
    #[inline]
    fn groom(&mut self) {
        self.0.groom();
        self.1.groom();
    }
}

impl<A: GroomField, B: GroomField, C: GroomField> GroomField for (A, B, C) {
    #[inline]
    fn groom(&mut self) {
        self.0.groom();
        self.1.groom();
        self.2.groom();
    }
}

// ---------------------------------------------------------------------------
// No-op leaves. Present so the derive can call uniformly without per-field
// carve-outs. Fields that genuinely need sweep-time cleanup add a custom
// `GroomField` impl or annotate the field with `#[groom(via = …)]`.
// ---------------------------------------------------------------------------

macro_rules! groom_noop {
    ($($t:ty),* $(,)?) => {
        $(
            impl GroomField for $t {
                #[inline]
                fn groom(&mut self) {}
            }
        )*
    };
}

groom_noop!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    f32,
    f64,
    String,
    &'static str,
);

impl<T: ?Sized> GroomField for std::sync::Arc<T> {
    /// Arc payloads in GC bodies own foreign data — sweep-time
    /// cleanup runs through their own `Drop`. The blanket impl is a
    /// no-op; if a body needs explicit Arc-level finalize work it
    /// reaches in through a hand-written `GroomField` impl.
    #[inline]
    fn groom(&mut self) {}
}

impl<T: ?Sized> GroomField for std::rc::Rc<T> {
    /// Same rationale as the `Arc<T>` no-op.
    #[inline]
    fn groom(&mut self) {}
}

impl<T> GroomField for otter_gc::Gc<T> {
    /// `Gc<T>` payloads have their own finalize / drop chain — the
    /// outer body does not own that storage.
    #[inline]
    fn groom(&mut self) {}
}

impl GroomField for crate::Value {
    /// `Value` is a NaN-boxed 8-byte word. The body holding it does
    /// not own the referenced object — leave finalize work to the
    /// referent's own [`otter_gc::SafeFinalize`] hook.
    #[inline]
    fn groom(&mut self) {}
}
