//! `JsStringGc` — per-type GC payload for ECMAScript string values.
//!
//! V8/JSC-parity tagged-variant string hierarchy:
//!
//! | Repr        | Storage                                  | Purpose                                  |
//! |-------------|------------------------------------------|------------------------------------------|
//! | `SeqOneByte`| `Box<[u8]>` Latin-1 / ASCII bytes        | Contiguous strings ≤ U+00FF              |
//! | `SeqTwoByte`| `Box<[u16]>` WTF-16 code units           | Contiguous strings (incl. lone surrogates)|
//! | `Cons`      | `(GcRef<JsStringGc>, GcRef<JsStringGc>)` | Lazy concat                              |
//! | `Sliced`    | `(GcRef<JsStringGc>, offset, length)`    | Lazy substring view                      |
//! | `Thin`      | `GcRef<JsStringGc>`                      | Forwarder after in-place flatten         |
//!
//! Cons / Sliced / Thin variants reference *other* `JsStringGc`
//! allocations via [`crate::gc_ref::GcRef`]. Those references must be
//! reported to the GC as field slots so the scavenger can update them
//! when the referenced object moves. See [`trace`] below.
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>
//!
//! # Layout / GC contract
//!
//! Every `JsStringGc` is allocated as
//! `[GcHeader (8 B) | JsStringGc (Self) | trailing payload]`. The
//! trailing payload is for the `SeqOneByte` / `SeqTwoByte`
//! flexible-array variants in a future "inline-FAM" perf pass — at
//! the moment those reprs use boxed slices and the trailing region is
//! empty.
//!
//! `align_of::<JsStringGc>() <= 8` is asserted at compile time.
//! Allocation goes through [`crate::heap::GcHeap::alloc_typed`] which
//! writes the [`crate::header::GcHeader`] before the payload.
//!
//! `repr` is `repr(u8)` to keep the discriminant compact; the variants
//! still occupy ≤ 24 B because their contents are at most a `Box<[u16]>`
//! (16 B on 64-bit) plus an offset / depth byte.

use std::sync::atomic::{AtomicU32, AtomicU8};

use crate::gc_ref::{GcRef, type_tag};
use crate::header::GcHeader;
use crate::heap::GcHeap;
use crate::trace::TraceFn;

/// Maximum string length in UTF-16 code units (mirrors V8's
/// `String::kMaxLength` on 64-bit: `(1 << 29) - 24`).
pub const MAX_STRING_LENGTH: u32 = (1 << 29) - 24;

/// Concat results of length ≤ this allocate a flat `Seq*` instead of
/// a `Cons` node. Below this threshold the bookkeeping overhead of
/// `Cons` outweighs the copy cost. V8 uses 13.
pub const MIN_CONS_LENGTH: u32 = 13;

/// Maximum cons depth before eagerly flattening. Bounds worst-case
/// flatten cost to O(n). V8 = 32.
pub const MAX_CONS_DEPTH: u8 = 32;

// -- Flag bits --------------------------------------------------------------

/// Bit 0: this string fits in Latin-1 (every code unit ≤ 0xFF).
pub const FLAG_ONE_BYTE: u8 = 0b0000_0001;
/// Bit 1: this string is the canonical interned representative.
pub const FLAG_INTERNALIZED: u8 = 0b0000_0010;
/// Bit 2: this string contains an unpaired surrogate.
pub const FLAG_LONE_SURROGATE: u8 = 0b0000_0100;

// -- Repr -------------------------------------------------------------------

/// Concrete string representation.
///
/// Cons / Sliced / Thin hold `GcRef<JsStringGc>` references; the trace
/// function emits one slot pointer per such field so the scavenger
/// can update them in place when the referenced string moves.
#[repr(u8)]
pub enum JsStringRepr {
    /// Contiguous Latin-1 / ASCII bytes (one byte == one UTF-16 unit).
    SeqOneByte(Box<[u8]>),

    /// Contiguous WTF-16 code units, including lone surrogates.
    SeqTwoByte(Box<[u16]>),

    /// Lazy concatenation node. `length = left.length + right.length`.
    /// Both children are GC-managed; the trace function emits two
    /// slot pointers (one per `GcRef` field).
    Cons {
        left: GcRef<JsStringGc>,
        right: GcRef<JsStringGc>,
        depth: u8,
    },

    /// Lazy substring view. Reads `[offset, offset + length)` of
    /// `parent`. The trace function emits one slot pointer for
    /// `parent`.
    Sliced {
        parent: GcRef<JsStringGc>,
        offset: u32,
    },

    /// Forwarder. After in-place flatten the original `Cons` /
    /// `Sliced` node rewrites itself to `Thin { forward }`. The trace
    /// function emits one slot pointer for `forward`.
    Thin { forward: GcRef<JsStringGc> },
}

// -- JsStringGc -------------------------------------------------------------

/// GC-managed ECMAScript string payload.
///
/// Stores the length, lazy hash cache, flags, and discriminated repr.
/// Header fields are hoisted out of the variants so `len()` /
/// `is_one_byte()` are branchless.
#[repr(C)]
pub struct JsStringGc {
    /// Length in UTF-16 code units. O(1) regardless of representation.
    /// `SeqOneByte` reports `len = byte_count` because each byte maps
    /// to one UTF-16 code unit.
    pub length: u32,

    /// Lazy FNV-1a hash; `0` is the sentinel "not computed".
    pub hash: AtomicU32,

    /// See `FLAG_*` constants.
    pub flags: AtomicU8,

    /// 3 bytes of explicit padding so the discriminated repr below
    /// starts on an 8-byte boundary regardless of compiler choices.
    /// Without this, `repr(C)` would still pad here; making it
    /// explicit lets the layout be obvious to readers.
    pub _padding: [u8; 3],

    /// Discriminated representation.
    pub repr: JsStringRepr,
}

// Compile-time layout assertions.
const _: () = assert!(
    std::mem::align_of::<JsStringGc>() <= 8,
    "JsStringGc alignment must be ≤ 8 (matches GcHeader alignment)",
);

impl JsStringGc {
    /// Returns the length in UTF-16 code units.
    #[inline]
    pub fn len(&self) -> usize {
        self.length as usize
    }

    /// Returns `true` if the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Returns `true` if the string fits in Latin-1.
    #[inline]
    pub fn is_one_byte(&self) -> bool {
        self.flags.load(std::sync::atomic::Ordering::Relaxed) & FLAG_ONE_BYTE != 0
    }

    /// Returns `true` if the string is interned (canonical for its content).
    #[inline]
    pub fn is_internalized(&self) -> bool {
        self.flags.load(std::sync::atomic::Ordering::Relaxed) & FLAG_INTERNALIZED != 0
    }

    /// Marks the string as the canonical interned representative.
    /// Idempotent; safe to call multiple times.
    #[inline]
    pub fn mark_internalized(&self) {
        self.flags
            .fetch_or(FLAG_INTERNALIZED, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns `true` if the string contains an unpaired surrogate.
    #[inline]
    pub fn contains_lone_surrogate(&self) -> bool {
        self.flags.load(std::sync::atomic::Ordering::Relaxed) & FLAG_LONE_SURROGATE != 0
    }

    /// Returns the cached FNV-1a hash, or `0` if not yet computed.
    #[inline]
    pub fn cached_hash(&self) -> u32 {
        self.hash.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Sets the cached hash. `0` is reserved as the "not computed"
    /// sentinel — callers must avoid storing `0`.
    #[inline]
    pub fn set_cached_hash(&self, value: u32) {
        debug_assert_ne!(value, 0, "0 is the cached_hash sentinel");
        self.hash
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }
}

// -- Trace function ---------------------------------------------------------

/// Trace function for `JsStringGc`. Emits one `*mut *const GcHeader`
/// slot pointer for every `GcRef<JsStringGc>` field stored in the
/// active variant.
///
/// `Seq*` variants are leaves — no slots emitted.
/// `Cons` emits two slots (left, right).
/// `Sliced` emits one slot (parent).
/// `Thin` emits one slot (forward).
///
/// The slot pointers point at the `NonNull<GcHeader>` field inside
/// the variant. Because [`GcRef<T>`] is `repr(transparent)` over
/// `NonNull<GcHeader>`, and `NonNull<GcHeader>` has the same memory
/// layout as `*const GcHeader`, the cast is valid.
///
/// # Safety
///
/// `header` must point at a live, header-prefixed `JsStringGc`
/// allocation. The variant tag in the payload must accurately reflect
/// the storage shape at the time of the call. Single-mutator
/// invariant means the payload cannot mutate while the trace runs.
fn trace(header: *const GcHeader, visit: &mut dyn FnMut(*mut *const GcHeader)) {
    // Compute pointer to the JsStringGc payload. Layout:
    //   [GcHeader (HEADER_SIZE) | JsStringGc payload]
    //
    // SAFETY: `header` points at a `[GcHeader | JsStringGc]`
    // allocation per the registration contract.
    let payload: *const JsStringGc = unsafe {
        (header as *const u8)
            .add(crate::header::HEADER_SIZE)
            .cast::<JsStringGc>()
    };

    // SAFETY: same as above; we read the discriminant + body of the
    // active variant.
    let repr_ref: &JsStringRepr = unsafe { &(*payload).repr };

    match repr_ref {
        JsStringRepr::SeqOneByte(_) | JsStringRepr::SeqTwoByte(_) => {
            // No GC pointer slots — leaf variant.
        }

        JsStringRepr::Cons { left, right, .. } => {
            // Slot pointers into the in-place `GcRef` fields.
            let left_slot = left as *const GcRef<JsStringGc> as *mut *const GcHeader;
            let right_slot = right as *const GcRef<JsStringGc> as *mut *const GcHeader;
            visit(left_slot);
            visit(right_slot);
        }

        JsStringRepr::Sliced { parent, .. } => {
            let parent_slot = parent as *const GcRef<JsStringGc> as *mut *const GcHeader;
            visit(parent_slot);
        }

        JsStringRepr::Thin { forward } => {
            let forward_slot = forward as *const GcRef<JsStringGc> as *mut *const GcHeader;
            visit(forward_slot);
        }
    }
}

const TRACE_FN: TraceFn = trace;

/// Registers the `JsStringGc` trace function under
/// [`type_tag::STRING`]. Called once by [`super::register_all`] per
/// fresh [`GcHeap`].
pub fn register(heap: &mut GcHeap) {
    heap.register_trace_fn(type_tag::STRING, TRACE_FN);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{GcConfig, GcHeap};
    use crate::local::HandleScope;

    fn fresh_heap() -> GcHeap {
        let mut heap = GcHeap::new(GcConfig {
            young_gen_size: 1024 * 1024,
            old_gen_threshold: 512 * 1024,
            ..GcConfig::default()
        });
        register(&mut heap);
        heap
    }

    fn alloc_seq_one_byte(scope: &mut HandleScope<'_>, bytes: &[u8]) -> GcRef<JsStringGc> {
        scope
            .alloc_typed(
                type_tag::STRING,
                JsStringGc {
                    length: bytes.len() as u32,
                    hash: AtomicU32::new(0),
                    flags: AtomicU8::new(FLAG_ONE_BYTE),
                    _padding: [0; 3],
                    repr: JsStringRepr::SeqOneByte(bytes.to_vec().into_boxed_slice()),
                },
            )
            .expect("alloc")
            .as_ref()
    }

    #[test]
    fn alloc_and_read_seq_one_byte() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let gc_ref = alloc_seq_one_byte(&mut scope, b"hello");

        let s = gc_ref.payload();
        assert_eq!(s.len(), 5);
        assert!(s.is_one_byte());
        match &s.repr {
            JsStringRepr::SeqOneByte(b) => assert_eq!(&**b, b"hello"),
            _ => panic!("expected SeqOneByte"),
        }
    }

    #[test]
    fn cons_node_emits_two_slots_during_trace() {
        let mut heap = fresh_heap();
        // Build cons("hi", "world") and trace it.
        let cons_ref = {
            let mut scope = HandleScope::new(&mut heap);
            let left = alloc_seq_one_byte(&mut scope, b"hi");
            let right = alloc_seq_one_byte(&mut scope, b"world");
            scope
                .alloc_typed(
                    type_tag::STRING,
                    JsStringGc {
                        length: 7,
                        hash: AtomicU32::new(0),
                        flags: AtomicU8::new(0),
                        _padding: [0; 3],
                        repr: JsStringRepr::Cons {
                            left,
                            right,
                            depth: 1,
                        },
                    },
                )
                .expect("alloc cons")
                .as_ref()
        };

        // Manually invoke the trace function and count slots.
        let mut emitted: Vec<*mut *const GcHeader> = Vec::new();
        trace(cons_ref.as_ptr().as_ptr() as *const GcHeader, &mut |slot| {
            emitted.push(slot);
        });
        assert_eq!(emitted.len(), 2, "cons must emit exactly two child slots");

        // Each slot must dereference to a non-null GcHeader pointer.
        for slot in &emitted {
            let ptr = unsafe { **slot };
            assert!(!ptr.is_null());
        }
    }

    #[test]
    fn sliced_emits_one_slot() {
        let mut heap = fresh_heap();
        let sliced_ref = {
            let mut scope = HandleScope::new(&mut heap);
            let parent = alloc_seq_one_byte(&mut scope, b"abcdef");
            scope
                .alloc_typed(
                    type_tag::STRING,
                    JsStringGc {
                        length: 4,
                        hash: AtomicU32::new(0),
                        flags: AtomicU8::new(FLAG_ONE_BYTE),
                        _padding: [0; 3],
                        repr: JsStringRepr::Sliced { parent, offset: 1 },
                    },
                )
                .expect("alloc sliced")
                .as_ref()
        };

        let mut count = 0;
        trace(
            sliced_ref.as_ptr().as_ptr() as *const GcHeader,
            &mut |_| count += 1,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn thin_emits_one_slot() {
        let mut heap = fresh_heap();
        let thin_ref = {
            let mut scope = HandleScope::new(&mut heap);
            let forward = alloc_seq_one_byte(&mut scope, b"target");
            scope
                .alloc_typed(
                    type_tag::STRING,
                    JsStringGc {
                        length: 6,
                        hash: AtomicU32::new(0),
                        flags: AtomicU8::new(FLAG_ONE_BYTE),
                        _padding: [0; 3],
                        repr: JsStringRepr::Thin { forward },
                    },
                )
                .expect("alloc thin")
                .as_ref()
        };

        let mut count = 0;
        trace(
            thin_ref.as_ptr().as_ptr() as *const GcHeader,
            &mut |_| count += 1,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn seq_variants_emit_no_slots() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let gc_ref = alloc_seq_one_byte(&mut scope, b"leaf");

        let mut count = 0;
        trace(gc_ref.as_ptr().as_ptr() as *const GcHeader, &mut |_| {
            count += 1;
        });
        assert_eq!(count, 0);
    }

    #[test]
    fn flags_round_trip() {
        let mut heap = fresh_heap();
        let mut scope = HandleScope::new(&mut heap);
        let r = alloc_seq_one_byte(&mut scope, b"x");
        let s = r.payload();
        assert!(s.is_one_byte());
        assert!(!s.is_internalized());
        s.mark_internalized();
        assert!(s.is_internalized());
        assert_eq!(s.cached_hash(), 0);
        s.set_cached_hash(42);
        assert_eq!(s.cached_hash(), 42);
    }
}
