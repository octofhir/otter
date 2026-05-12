//! JavaScript `ArrayBuffer` (ECMA-262 §25.1).
//!
//! Backing store is a heap-shared `RefCell<Vec<u8>>`; cloning a
//! `JsArrayBuffer` shares the same byte buffer, matching spec
//! mutation semantics. The `detached` flag is interior-mutable
//! through a `Cell<bool>` so transfer / detach operations are
//! observable through every clone of the handle.
//!
//! # Contents
//! - [`JsArrayBuffer`] — cheap-to-clone handle.
//! - [`ArrayBufferBody`] — internal storage.
//!
//! # Invariants
//! - When `detached == true`, the byte buffer is empty. Every
//!   operation that needs the bytes must check
//!   [`JsArrayBuffer::is_detached`] first per §25.1.3.1
//!   `IsDetachedBuffer`.
//! - For resizable buffers, `max_byte_length` is `Some(n)` and the
//!   underlying `Vec<u8>` capacity is at least `n`. Length within
//!   the buffer floats between `0..=max_byte_length` via
//!   [`JsArrayBuffer::resize`].
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-isdetachedbuffer>

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::rc::Rc;

/// Cheap-to-clone `ArrayBuffer` handle.
#[derive(Debug, Clone)]
pub struct JsArrayBuffer {
    inner: Rc<ArrayBufferBody>,
}

/// Internal storage for an `ArrayBuffer`.
#[derive(Debug)]
pub struct ArrayBufferBody {
    /// Raw bytes. Empty when detached.
    bytes: RefCell<Vec<u8>>,
    /// `true` after detach / transfer; once set, stays set per spec.
    detached: Cell<bool>,
    /// `Some(n)` for a resizable buffer; `None` for a fixed-length
    /// buffer. When set, [`Self::bytes`] never grows beyond `n`.
    max_byte_length: Option<usize>,
    /// `true` for `SharedArrayBuffer` per ECMA-262 §25.2.
    /// SharedArrayBuffer cannot be detached and exposes `.grow`
    /// instead of `.resize` per §25.2.5. The single-threaded
    /// foundation shares storage by `Rc` like an ordinary buffer;
    /// real cross-isolate sharing arrives with the worker subset.
    shared: bool,
}

impl JsArrayBuffer {
    /// Allocate a fresh fixed-length buffer of `len` zero bytes.
    /// `len` must already be a valid `usize` (the dispatcher honours
    /// §25.1.2.1 ToIndex on the user-facing argument).
    ///
    /// Returns the empty buffer when `len` exceeds practical limits
    /// — the [`JsArrayBuffer::try_new`] entry point preserves the
    /// fallible shape for ctors that need to surface a RangeError.
    /// This infallible constructor is kept for callers that know
    /// the length is bounded.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self::try_new(len).unwrap_or_else(|| Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(Vec::new()),
                detached: Cell::new(true),
                max_byte_length: None,
                shared: false,
            }),
        })
    }

    /// Fallible variant of [`Self::new`]. Uses `Vec::try_reserve`
    /// so the dispatcher can surface a `RangeError` for the spec
    /// §25.1.2.1 step 5 too-big case (and, in practice, for any
    /// allocation that exceeds the process memory budget).
    #[must_use]
    pub fn try_new(len: usize) -> Option<Self> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0u8);
        Some(Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(bytes),
                detached: Cell::new(false),
                max_byte_length: None,
                shared: false,
            }),
        })
    }

    /// Allocate a resizable buffer with initial length `len` and the
    /// given upper bound. Capacity is reserved up-front so subsequent
    /// `resize` calls never reallocate.
    #[must_use]
    pub fn new_resizable(len: usize, max_byte_length: usize) -> Self {
        let mut bytes = Vec::with_capacity(max_byte_length);
        bytes.resize(len, 0u8);
        Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(bytes),
                detached: Cell::new(false),
                max_byte_length: Some(max_byte_length),
                shared: false,
            }),
        }
    }

    /// Wrap an existing byte vector. Used by [`JsArrayBuffer::slice`]
    /// and `transfer` / `transferToFixedLength`.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(bytes),
                detached: Cell::new(false),
                max_byte_length: None,
                shared: false,
            }),
        }
    }

    /// Allocate a fixed-length `SharedArrayBuffer`. Cannot be
    /// detached; differs from an ordinary `ArrayBuffer` only by
    /// the [`Self::is_shared`] flag in the single-threaded
    /// foundation surface.
    ///
    /// Returns a synthetic detached buffer when the allocation
    /// fails; [`Self::try_new_shared`] preserves the fallible
    /// shape for callers that need to surface a `RangeError`.
    #[must_use]
    pub fn new_shared(len: usize) -> Self {
        Self::try_new_shared(len).unwrap_or_else(|| Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(Vec::new()),
                detached: Cell::new(true),
                max_byte_length: None,
                shared: true,
            }),
        })
    }

    /// Fallible variant of [`Self::new_shared`]. Uses
    /// `Vec::try_reserve_exact` so the dispatcher can surface a
    /// `RangeError` when `len` exceeds the process memory budget.
    #[must_use]
    pub fn try_new_shared(len: usize) -> Option<Self> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0u8);
        Some(Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(bytes),
                detached: Cell::new(false),
                max_byte_length: None,
                shared: true,
            }),
        })
    }

    /// Allocate a growable shared buffer per §25.2.5 — `length`
    /// floats up to `max_byte_length` via [`Self::grow`].
    #[must_use]
    pub fn new_shared_growable(len: usize, max_byte_length: usize) -> Self {
        let mut bytes = Vec::with_capacity(max_byte_length);
        bytes.resize(len, 0u8);
        Self {
            inner: Rc::new(ArrayBufferBody {
                bytes: RefCell::new(bytes),
                detached: Cell::new(false),
                max_byte_length: Some(max_byte_length),
                shared: true,
            }),
        }
    }

    /// `true` for a `SharedArrayBuffer`.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.inner.shared
    }

    /// `true` for a growable `SharedArrayBuffer` (the SAB
    /// equivalent of resizable).
    #[must_use]
    pub fn is_growable(&self) -> bool {
        self.inner.shared && self.inner.max_byte_length.is_some()
    }

    /// §25.2.5.4 — `SharedArrayBuffer.prototype.grow(newByteLength)`.
    /// Growing only; `new_len < current_len` returns `false`.
    pub fn grow(&self, new_len: usize) -> bool {
        if !self.is_growable() {
            return false;
        }
        let max = match self.inner.max_byte_length {
            Some(m) => m,
            None => return false,
        };
        if new_len > max {
            return false;
        }
        let mut bytes = self.inner.bytes.borrow_mut();
        if new_len < bytes.len() {
            return false;
        }
        bytes.resize(new_len, 0u8);
        true
    }

    /// Current byte length. `0` for a detached buffer.
    #[must_use]
    pub fn byte_length(&self) -> usize {
        if self.is_detached() {
            return 0;
        }
        self.inner.bytes.borrow().len()
    }

    /// Maximum byte length for a resizable buffer; equals
    /// [`Self::byte_length`] for a fixed-length buffer per
    /// §25.1.4.6 `get ArrayBuffer.prototype.maxByteLength`.
    #[must_use]
    pub fn max_byte_length(&self) -> usize {
        if self.is_detached() {
            return 0;
        }
        self.inner
            .max_byte_length
            .unwrap_or_else(|| self.inner.bytes.borrow().len())
    }

    /// `true` when the buffer was constructed with a `maxByteLength`
    /// argument (§25.1.4.7 `get ArrayBuffer.prototype.resizable`).
    #[must_use]
    pub fn is_resizable(&self) -> bool {
        self.inner.max_byte_length.is_some()
    }

    /// `true` once detach / transfer has happened (§25.1.3.1
    /// `IsDetachedBuffer`).
    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.inner.detached.get()
    }

    /// Borrow the bytes read-only. Callers must check
    /// [`Self::is_detached`] first.
    #[must_use]
    pub fn borrow_bytes(&self) -> Ref<'_, Vec<u8>> {
        self.inner.bytes.borrow()
    }

    /// Borrow the bytes mutably. Callers must check
    /// [`Self::is_detached`] first.
    #[must_use]
    pub fn borrow_bytes_mut(&self) -> RefMut<'_, Vec<u8>> {
        self.inner.bytes.borrow_mut()
    }

    /// Detach the buffer. Idempotent; subsequent calls are no-ops.
    /// `SharedArrayBuffer` rejects detach per §25.2.4.1 step 2 —
    /// the call is a no-op there.
    pub fn detach(&self) {
        if self.inner.shared {
            return;
        }
        if !self.inner.detached.replace(true) {
            self.inner.bytes.borrow_mut().clear();
        }
    }

    /// Resize a resizable buffer. Returns `false` when the buffer is
    /// fixed-length, detached, or `new_len` exceeds the recorded
    /// `maxByteLength`. Length growth zero-fills new bytes per
    /// §25.1.4.4 step 8.
    pub fn resize(&self, new_len: usize) -> bool {
        if self.is_detached() {
            return false;
        }
        let max = match self.inner.max_byte_length {
            Some(m) => m,
            None => return false,
        };
        if new_len > max {
            return false;
        }
        let mut bytes = self.inner.bytes.borrow_mut();
        bytes.resize(new_len, 0u8);
        true
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// `Rc` data-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        Rc::as_ptr(&self.inner).cast()
    }
}

impl PartialEq for JsArrayBuffer {
    fn eq(&self, other: &Self) -> bool {
        // ECMAScript `===` on ArrayBuffer values follows reference
        // identity per the object-equality wildcard arm in
        // [`crate::Value::PartialEq`]; this implementation is
        // consistent.
        self.ptr_eq(other)
    }
}
