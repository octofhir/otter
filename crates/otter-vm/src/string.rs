//! WTF-16 backed JavaScript string with rope variants.
//!
//! Implements the active string model:
//!
//! - canonical storage is WTF-16 (`Arc<[u16]>`); we never round-trip
//!   through UTF-8 internally;
//! - concatenation produces a `Cons` rope node — never an eager
//!   flat copy — so `s += piece` loops stay O(n);
//! - slices produce `Sliced` views (over flat parents) without
//!   flattening; slicing a `Cons` flattens once;
//! - `Thin` variant is reserved for the future Latin-1 / WTF-16
//!   hybrid (not constructed yet; tag occupies the enum discriminant
//!   so it cannot be repurposed casually);
//! - heap accounting goes through a fallible `alloc_string` helper
//!   that checks the runtime cap **before** mutation and returns
//!   `OutOfMemory` if the allocation would exceed it.
//!
//! Ropes are flattened with an **iterative DFS** over an explicit
//! stack — recursion is forbidden by the foundation plan.
//!
//! # Contents
//! - [`JsString`] — the public string handle (cheap to clone).
//! - [`StringRepr`] — internal representation enum.
//! - [`StringHeap`] — bytes-tracking heap accountant.
//! - [`StringError`] — fallible allocation outcome.
//! - [`MAX_ROPE_DEPTH`] — pinned at 64 (panicking flatten cap).
//!
//! # Invariants
//! - `len()` is O(1) for every variant.
//! - `equals()` compares code units; surrogates round-trip.
//! - `slice(parent, start, len)` over a `Sliced` parent collapses
//!   into a single `Sliced` view (no `Sliced(Sliced(...))` chain).
//! - The string subsystem allocates **only** `Arc<[u16]>`; no
//!   `String` or `Vec<u8>` heap allocation.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Per-variant header overhead used by the heap accountant. The
/// numbers are conservative estimates; real Rust layout may be
/// smaller. They exist so the heap counter tracks something
/// meaningful for `Cons` / `Sliced` nodes that would otherwise look
/// "free" relative to flat strings.
const FLAT_HEADER_BYTES: u64 = 24;
const CONS_HEADER_BYTES: u64 = 32;
const SLICED_HEADER_BYTES: u64 = 32;

/// Maximum depth of an unflattened cons rope.
///
/// Operations that walk a rope (`charCodeAt`, `slice`, `flatten`)
/// use an explicit stack capped at this depth. Concatenations that
/// would exceed it trigger an eager flatten before the new `Cons`
/// node is built, so the depth can never exceed [`MAX_ROPE_DEPTH`]
/// in steady state.
pub const MAX_ROPE_DEPTH: usize = 64;

/// Outcome of a fallible string allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StringError {
    /// The allocation would have exceeded the configured heap cap.
    OutOfMemory {
        /// Bytes the allocation requested.
        requested_bytes: u64,
        /// Configured heap limit (`0` means "disabled").
        heap_limit_bytes: u64,
    },
}

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => write!(
                f,
                "out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}"
            ),
        }
    }
}

impl std::error::Error for StringError {}

/// Tracks live string heap bytes against an optional cap.
///
/// `Default` produces a "no-cap" heap (cap = 0 disables the check).
/// The accountant is `Send + Sync` so an `InterruptHandle`-style
/// shared counter is straightforward when foundation slices wire it
/// to the runtime.
#[derive(Debug, Default)]
pub struct StringHeap {
    used: AtomicU64,
    cap: AtomicU64,
}

impl StringHeap {
    /// Construct a heap with an explicit cap (`0` disables).
    #[must_use]
    pub fn with_cap(cap_bytes: u64) -> Self {
        Self {
            used: AtomicU64::new(0),
            cap: AtomicU64::new(cap_bytes),
        }
    }

    /// Currently tracked live bytes.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Configured cap (`0` = unlimited).
    #[must_use]
    pub fn cap(&self) -> u64 {
        self.cap.load(Ordering::Relaxed)
    }

    /// Reserve `bytes`; returns [`StringError::OutOfMemory`] when
    /// the cap would be exceeded. Never mutates the counter on
    /// failure (foundation plan §"Heap caps are hard").
    pub fn reserve(&self, bytes: u64) -> Result<(), StringError> {
        let cap = self.cap.load(Ordering::Relaxed);
        if cap == 0 {
            self.used.fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        }
        // CAS-loop so the check + mutation is atomic relative to
        // other concurrent allocations (single-threaded VM today,
        // but the heap is `Sync` so we keep the property).
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let new = current.saturating_add(bytes);
            if new > cap {
                return Err(StringError::OutOfMemory {
                    requested_bytes: bytes,
                    heap_limit_bytes: cap,
                });
            }
            match self.used.compare_exchange_weak(
                current,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    /// Release `bytes` previously reserved.
    pub fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Internal representation of a [`JsString`].
///
/// All variants are cheap to clone — heavy data lives behind
/// [`Arc`].
#[derive(Debug, Clone)]
pub enum StringRepr {
    /// Flat WTF-16 storage. The canonical leaf form.
    Flat(Arc<[u16]>),
    /// Concatenation rope node. Length is precomputed.
    Cons {
        /// Left child.
        left: Arc<JsString>,
        /// Right child.
        right: Arc<JsString>,
        /// Total code-unit length (`left.len() + right.len()`).
        len: u32,
        /// Maximum depth of either child plus one.
        depth: u8,
    },
    /// Slice view over a `Flat` parent. Slicing a `Cons` flattens
    /// the parent before producing this variant.
    Sliced {
        /// Parent storage. Only `Flat` variants are referenced
        /// here so indexing stays O(1).
        parent: Arc<[u16]>,
        /// Start offset (code units) into `parent`.
        start: u32,
        /// Length (code units).
        len: u32,
    },
    /// Latin-1 storage. Each byte zero-extends to a `u16` code
    /// unit on read. Used as the inline target of ASCII-only
    /// constructors (e.g. numeric formatters in
    /// `crate::number::ecma`) so the result avoids the
    /// `&str → Vec<u16> → Arc<[u16]>` widening round-trip that
    /// `from_str` performs.
    Thin(Arc<[u8]>),
}

/// Cheap, cloneable JavaScript string handle.
#[derive(Debug, Clone)]
pub struct JsString {
    repr: Arc<StringRepr>,
}

impl JsString {
    /// Construct a flat string from in-memory WTF-16 code units.
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if `heap` cannot
    /// accommodate the allocation.
    pub fn from_utf16_units(units: &[u16], heap: &StringHeap) -> Result<Self, StringError> {
        let bytes = FLAT_HEADER_BYTES + (units.len() as u64) * 2;
        heap.reserve(bytes)?;
        Ok(Self {
            repr: Arc::new(StringRepr::Flat(units.into())),
        })
    }

    /// Construct a flat string from a Rust `&str`. Convenience for
    /// literal loading; the conversion to WTF-16 happens once.
    ///
    /// # Errors
    /// See [`Self::from_utf16_units`].
    pub fn from_str(s: &str, heap: &StringHeap) -> Result<Self, StringError> {
        let units: Vec<u16> = s.encode_utf16().collect();
        Self::from_utf16_units(&units, heap)
    }

    /// Construct a Latin-1-tagged string from an ASCII / Latin-1
    /// byte slice. Each byte zero-extends to a `u16` code unit on
    /// read; storage stays a single `Arc<[u8]>` to avoid the
    /// `&str → Vec<u16>` widening allocation that
    /// [`Self::from_str`] performs.
    ///
    /// Caller is responsible for ensuring `bytes` are valid Latin-1
    /// (every byte ≤ `0xFF` is trivially Latin-1, but ASCII-only
    /// callers preserve the spec semantics for code-unit access).
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if `heap` cannot
    /// accommodate the allocation.
    pub fn from_latin1(bytes: &[u8], heap: &StringHeap) -> Result<Self, StringError> {
        let alloc = FLAT_HEADER_BYTES + bytes.len() as u64;
        heap.reserve(alloc)?;
        Ok(Self {
            repr: Arc::new(StringRepr::Thin(bytes.into())),
        })
    }

    /// Empty string convenience constructor (no allocation
    /// accounting beyond the header).
    ///
    /// # Errors
    /// See [`Self::from_utf16_units`].
    pub fn empty(heap: &StringHeap) -> Result<Self, StringError> {
        Self::from_utf16_units(&[], heap)
    }

    /// Borrow the underlying Latin-1 byte storage iff this string
    /// is the [`StringRepr::Thin`] variant. Returns `None` for
    /// flat / sliced / cons UTF-16 strings.
    ///
    /// Callers exploit this for byte-level fast paths — substring
    /// search, prefix / suffix tests — that would otherwise pay
    /// the [`Self::to_utf16_vec`] widening allocation.
    #[must_use]
    pub fn as_latin1(&self) -> Option<&[u8]> {
        match &*self.repr {
            StringRepr::Thin(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Length in WTF-16 code units (O(1)).
    #[must_use]
    pub fn len(&self) -> u32 {
        match &*self.repr {
            StringRepr::Flat(units) => units.len() as u32,
            StringRepr::Cons { len, .. } | StringRepr::Sliced { len, .. } => *len,
            StringRepr::Thin(bytes) => bytes.len() as u32,
        }
    }

    /// `true` when [`Self::len`] is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Concatenate two strings into a `Cons` rope node.
    ///
    /// Cheap: the operation is bounded by the depth-bound check
    /// plus an `Arc::clone` per child. If either operand pushes the
    /// resulting depth above [`MAX_ROPE_DEPTH`], the deeper child
    /// is flattened first.
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if the heap cannot
    /// accommodate the rope-node header overhead, or the eager
    /// flatten if one is required.
    pub fn concat(
        left: &JsString,
        right: &JsString,
        heap: &StringHeap,
    ) -> Result<Self, StringError> {
        if right.is_empty() {
            return Ok(left.clone());
        }
        if left.is_empty() {
            return Ok(right.clone());
        }
        let new_len = left.len().saturating_add(right.len());
        let new_depth = left.depth().max(right.depth()).saturating_add(1);

        // Force a flatten of the deeper side if we are about to
        // exceed the depth budget.
        let (l, r) = if (new_depth as usize) > MAX_ROPE_DEPTH {
            if left.depth() >= right.depth() {
                let flat = left.flatten(heap)?;
                (flat, right.clone())
            } else {
                let flat = right.flatten(heap)?;
                (left.clone(), flat)
            }
        } else {
            (left.clone(), right.clone())
        };

        let final_depth = l.depth().max(r.depth()).saturating_add(1);
        heap.reserve(CONS_HEADER_BYTES)?;
        Ok(Self {
            repr: Arc::new(StringRepr::Cons {
                left: Arc::new(l),
                right: Arc::new(r),
                len: new_len,
                depth: final_depth,
            }),
        })
    }

    /// Take an O(len) substring view.
    ///
    /// `start` is clamped to `[0, len()]`, `length` to
    /// `[0, len() - start]`. Slicing a `Cons` parent flattens it
    /// first; slicing a `Sliced` parent collapses to a single
    /// `Sliced` view.
    ///
    /// # Errors
    /// See [`Self::flatten`].
    pub fn slice(&self, start: u32, length: u32, heap: &StringHeap) -> Result<Self, StringError> {
        let total = self.len();
        let start = start.min(total);
        let length = length.min(total.saturating_sub(start));
        if length == 0 {
            return Self::empty(heap);
        }
        match &*self.repr {
            StringRepr::Flat(units) => {
                heap.reserve(SLICED_HEADER_BYTES)?;
                Ok(Self {
                    repr: Arc::new(StringRepr::Sliced {
                        parent: units.clone(),
                        start,
                        len: length,
                    }),
                })
            }
            StringRepr::Sliced {
                parent,
                start: outer_start,
                ..
            } => {
                heap.reserve(SLICED_HEADER_BYTES)?;
                Ok(Self {
                    repr: Arc::new(StringRepr::Sliced {
                        parent: parent.clone(),
                        start: outer_start + start,
                        len: length,
                    }),
                })
            }
            StringRepr::Cons { .. } => {
                let flat = self.flatten(heap)?;
                flat.slice(start, length, heap)
            }
            StringRepr::Thin(bytes) => {
                // Latin-1 source: collapse the slice into a fresh
                // `Thin` rather than widening to WTF-16. Keeps the
                // 1-byte-per-code-unit advantage on the slice path.
                let s = start as usize;
                let e = s + (length as usize);
                let alloc = FLAT_HEADER_BYTES + (length as u64);
                heap.reserve(alloc)?;
                Ok(Self {
                    repr: Arc::new(StringRepr::Thin(bytes[s..e].into())),
                })
            }
        }
    }

    /// Realize a rope into a flat representation. O(n) over the
    /// length; iterative DFS — no recursion.
    ///
    /// # Errors
    /// Returns [`StringError::OutOfMemory`] if the destination
    /// allocation would exceed the heap cap.
    pub fn flatten(&self, heap: &StringHeap) -> Result<Self, StringError> {
        if let StringRepr::Flat(_) = &*self.repr {
            return Ok(self.clone());
        }
        let mut buf: Vec<u16> = Vec::with_capacity(self.len() as usize);
        // Iterative DFS over the rope; each `Cons` pushes its right
        // child to the stack and descends left.
        let mut stack: Vec<&JsString> = Vec::with_capacity(MAX_ROPE_DEPTH);
        stack.push(self);
        while let Some(node) = stack.pop() {
            match &*node.repr {
                StringRepr::Flat(units) => buf.extend_from_slice(units),
                StringRepr::Sliced { parent, start, len } => {
                    let s = *start as usize;
                    let e = s + (*len as usize);
                    buf.extend_from_slice(&parent[s..e]);
                }
                StringRepr::Cons { left, right, .. } => {
                    // Push right first so left is processed first.
                    stack.push(right);
                    stack.push(left);
                }
                StringRepr::Thin(bytes) => {
                    buf.extend(bytes.iter().map(|&b| u16::from(b)));
                }
            }
        }
        Self::from_utf16_units(&buf, heap)
    }

    /// `true` when the two strings have identical code units.
    ///
    /// Fast paths:
    /// - identity (`Arc::ptr_eq` on the inner repr);
    /// - identical `Flat` storages;
    /// - length mismatch returns `false` immediately.
    #[must_use]
    pub fn equals(&self, other: &JsString) -> bool {
        if Arc::ptr_eq(&self.repr, &other.repr) {
            return true;
        }
        if self.len() != other.len() {
            return false;
        }
        if let (StringRepr::Flat(a), StringRepr::Flat(b)) = (&*self.repr, &*other.repr) {
            return Arc::ptr_eq(a, b) || a == b;
        }
        // General path: walk both ropes via iterators.
        let mut a = CodeUnits::new(self);
        let mut b = CodeUnits::new(other);
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => continue,
                (None, None) => return true,
                _ => return false,
            }
        }
    }

    /// `Display`/`Debug`-style rendering as a Rust `String`.
    ///
    /// Lone surrogates are preserved through
    /// [`String::from_utf16_lossy`] semantics. Used by the CLI
    /// formatter at the stdout boundary; **not** used internally.
    #[must_use]
    pub fn to_lossy_string(&self) -> String {
        let units: Vec<u16> = CodeUnits::new(self).collect();
        String::from_utf16_lossy(&units)
    }

    /// Collect the string into a freshly-allocated `Vec<u16>` of
    /// code units. Useful for bytecode dumps and golden tests.
    #[must_use]
    pub fn to_utf16_vec(&self) -> Vec<u16> {
        CodeUnits::new(self).collect()
    }

    /// Code-unit at `index`, or `None` for out-of-range.
    #[must_use]
    pub fn char_code_at(&self, index: u32) -> Option<u16> {
        if index >= self.len() {
            return None;
        }
        // Iterative descent into the rope.
        let mut node = self.clone();
        let mut idx = index;
        loop {
            match Arc::clone(&node.repr).as_ref() {
                StringRepr::Flat(units) => return Some(units[idx as usize]),
                StringRepr::Sliced { parent, start, .. } => {
                    return Some(parent[(*start + idx) as usize]);
                }
                StringRepr::Cons { left, right, .. } => {
                    let left_len = left.len();
                    if idx < left_len {
                        node = (**left).clone();
                    } else {
                        idx -= left_len;
                        node = (**right).clone();
                    }
                }
                StringRepr::Thin(bytes) => {
                    return Some(u16::from(bytes[idx as usize]));
                }
            }
        }
    }

    fn depth(&self) -> u8 {
        match &*self.repr {
            StringRepr::Flat(_) | StringRepr::Sliced { .. } => 0,
            StringRepr::Cons { depth, .. } => *depth,
            StringRepr::Thin(_) => 0,
        }
    }

    /// Find `needle` starting at code-unit `from`. Returns the
    /// match position (in code units) or `None`.
    ///
    /// `interrupt` is consulted every
    /// [`INDEX_OF_INTERRUPT_BUDGET`] iterations; a tripped flag
    /// produces an [`Interrupted`] sentinel which callers translate
    /// to `VmError::Interrupted`.
    ///
    /// Hot path uses [`crate::swar`] to scan 8 bytes (Latin-1) or
    /// 4 code units (UTF-16) at a time when locating candidate
    /// match positions, falling back to slice equality for the
    /// per-candidate verify step.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.indexof>
    /// §22.1.3.10
    pub fn index_of(
        &self,
        needle: &JsString,
        from: u32,
        interrupt: Option<&crate::InterruptFlag>,
    ) -> Result<Option<u32>, Interrupted> {
        let n_len = needle.len();
        if n_len == 0 {
            return Ok(Some(from.min(self.len())));
        }
        let h_len = self.len();
        if h_len < n_len {
            return Ok(None);
        }
        let last_start = h_len - n_len;
        if from > last_start {
            return Ok(None);
        }

        // Both sides Latin-1 → byte-level scan (no `Vec<u16>`
        // materialisation, SWAR memchr on the first byte).
        if let (Some(h_bytes), Some(n_bytes)) = (self.as_latin1(), needle.as_latin1()) {
            return latin1_index_of(
                h_bytes,
                n_bytes,
                from as usize,
                last_start as usize,
                interrupt,
            );
        }

        // UTF-16 path: still materialise to flat code-unit slices
        // so the verify step stays a single `==` comparison, but
        // route the candidate scan through the SWAR `find_u16`
        // helper (4 lanes per `u64`).
        let haystack: Vec<u16> = self.to_utf16_vec();
        let needle_units: Vec<u16> = needle.to_utf16_vec();
        utf16_index_of(
            &haystack,
            &needle_units,
            from as usize,
            last_start as usize,
            interrupt,
        )
    }

    /// Find the **last** occurrence of `needle` ending at or
    /// before code-unit position `position` (exclusive upper
    /// bound is `position + needle.len()`). Returns `None` if no
    /// match exists.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.lastindexof>
    /// §22.1.3.11
    pub fn last_index_of(
        &self,
        needle: &JsString,
        position: u32,
        interrupt: Option<&crate::InterruptFlag>,
    ) -> Result<Option<u32>, Interrupted> {
        let n_len = needle.len();
        let h_len = self.len();
        if n_len == 0 {
            return Ok(Some(position.min(h_len)));
        }
        if h_len < n_len {
            return Ok(None);
        }
        let max_start = h_len - n_len;
        let last_start = position.min(max_start);

        if let (Some(h_bytes), Some(n_bytes)) = (self.as_latin1(), needle.as_latin1()) {
            return latin1_last_index_of(h_bytes, n_bytes, last_start as usize, interrupt);
        }

        let haystack: Vec<u16> = self.to_utf16_vec();
        let needle_units: Vec<u16> = needle.to_utf16_vec();
        utf16_last_index_of(&haystack, &needle_units, last_start as usize, interrupt)
    }

    /// `true` when this string starts with `prefix` at offset
    /// `from`. Cheap: pulls only the relevant code units.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-string.prototype.startswith>
    /// §22.1.3.22
    #[must_use]
    pub fn starts_with(&self, prefix: &JsString, from: u32) -> bool {
        let p_len = prefix.len();
        if p_len == 0 {
            return true;
        }
        if from + p_len > self.len() {
            return false;
        }
        // Both Latin-1 → byte-level slice equality (compiler
        // vectorises the memcmp). Skips the `Vec<u16>` widening
        // both sides currently pay.
        if let (Some(h), Some(p)) = (self.as_latin1(), prefix.as_latin1()) {
            let from = from as usize;
            let p_len = p_len as usize;
            return h[from..from + p_len] == *p;
        }
        // Mixed / UTF-16: materialise once and compare slices.
        let haystack: Vec<u16> = self.to_utf16_vec();
        let prefix_units: Vec<u16> = prefix.to_utf16_vec();
        let from = from as usize;
        let p_len = p_len as usize;
        haystack[from..from + p_len] == prefix_units[..]
    }

    /// `true` when this string ends with `suffix`. `end_position`
    /// caps the haystack to the first `end_position` code units —
    /// matches `String.prototype.endsWith` semantics.
    #[must_use]
    pub fn ends_with(&self, suffix: &JsString, end_position: u32) -> bool {
        let total = self.len().min(end_position);
        let s_len = suffix.len();
        if s_len > total {
            return false;
        }
        self.starts_with(suffix, total - s_len)
    }

    /// Lexicographic code-unit comparison used by `<`, `<=`, `>`,
    /// `>=` for two strings. Returns `Less`/`Equal`/`Greater`.
    #[must_use]
    pub fn compare_lex(&self, other: &JsString) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let mut a = CodeUnits::new(self);
        let mut b = CodeUnits::new(other);
        loop {
            match (a.next(), b.next()) {
                (Some(x), Some(y)) => match x.cmp(&y) {
                    Ordering::Equal => continue,
                    other => return other,
                },
                (None, None) => return Ordering::Equal,
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
            }
        }
    }
}

/// Loop-iteration budget at which `index_of` checks the interrupt
/// flag. Matches the foundation plan's "every 4096 iterations" rule
/// for native loops.
pub const INDEX_OF_INTERRUPT_BUDGET: u32 = 4096;

/// Latin-1 index_of fast path: SWAR scan for the needle's first
/// byte, then verify the candidate via slice equality.
fn latin1_index_of(
    haystack: &[u8],
    needle: &[u8],
    from: usize,
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start < haystack.len());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_start = from;
    let mut steps: u32 = 0;
    while search_start <= last_start {
        let Some(rel) = crate::swar::find_byte(&haystack[search_start..=last_start], first, 0)
        else {
            return Ok(None);
        };
        let i = search_start + rel;
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add(rel as u32 + 1);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        search_start = i + 1;
    }
    Ok(None)
}

/// Latin-1 last_index_of fast path: SWAR rfind for the needle's
/// first byte, then verify the candidate via slice equality.
fn latin1_last_index_of(
    haystack: &[u8],
    needle: &[u8],
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_end = last_start + 1;
    let mut steps: u32 = 0;
    while search_end > 0 {
        let Some(i) = crate::swar::rfind_byte(&haystack[..search_end], first) else {
            return Ok(None);
        };
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add((search_end - i) as u32);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        if i == 0 {
            return Ok(None);
        }
        search_end = i;
    }
    Ok(None)
}

/// UTF-16 last_index_of with SWAR-assisted reverse candidate
/// scan.
fn utf16_last_index_of(
    haystack: &[u16],
    needle: &[u16],
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_end = last_start + 1;
    let mut steps: u32 = 0;
    while search_end > 0 {
        let Some(i) = crate::swar::rfind_u16(&haystack[..search_end], first) else {
            return Ok(None);
        };
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add((search_end - i) as u32);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        if i == 0 {
            return Ok(None);
        }
        search_end = i;
    }
    Ok(None)
}

/// UTF-16 index_of with SWAR-assisted candidate scan.
fn utf16_index_of(
    haystack: &[u16],
    needle: &[u16],
    from: usize,
    last_start: usize,
    interrupt: Option<&crate::InterruptFlag>,
) -> Result<Option<u32>, Interrupted> {
    debug_assert!(!needle.is_empty());
    debug_assert!(last_start < haystack.len());
    debug_assert!(last_start + needle.len() <= haystack.len());
    let first = needle[0];
    let n_len = needle.len();
    let mut search_start = from;
    let mut steps: u32 = 0;
    while search_start <= last_start {
        let Some(rel) = crate::swar::find_u16(&haystack[search_start..=last_start], first, 0)
        else {
            return Ok(None);
        };
        let i = search_start + rel;
        if haystack[i..i + n_len] == *needle {
            return Ok(Some(i as u32));
        }
        steps = steps.saturating_add(rel as u32 + 1);
        if steps >= INDEX_OF_INTERRUPT_BUDGET {
            if let Some(flag) = interrupt
                && flag.is_set()
            {
                return Err(Interrupted);
            }
            steps = 0;
        }
        search_start = i + 1;
    }
    Ok(None)
}

/// Sentinel returned by [`JsString::index_of`] when the runtime
/// interrupt flag was observed. Carries no payload — callers
/// translate it to `VmError::Interrupted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for JsString {}

/// Iterator over a rope's code units. Iterative — uses an
/// explicit stack capped at [`MAX_ROPE_DEPTH`].
struct CodeUnits<'a> {
    stack: Vec<NodeFrame<'a>>,
}

enum NodeFrame<'a> {
    Flat { slice: &'a [u16], pos: usize },
    Sliced { slice: &'a [u16] },
    Cons { right: &'a JsString },
    Latin1 { slice: &'a [u8] },
}

impl<'a> CodeUnits<'a> {
    fn new(s: &'a JsString) -> Self {
        let mut iter = Self {
            stack: Vec::with_capacity(MAX_ROPE_DEPTH),
        };
        iter.push(s);
        iter
    }

    fn push(&mut self, s: &'a JsString) {
        let mut current = s;
        loop {
            match &*current.repr {
                StringRepr::Flat(units) => {
                    self.stack.push(NodeFrame::Flat {
                        slice: units,
                        pos: 0,
                    });
                    return;
                }
                StringRepr::Sliced { parent, start, len } => {
                    let s = *start as usize;
                    let e = s + (*len as usize);
                    self.stack.push(NodeFrame::Sliced {
                        slice: &parent[s..e],
                    });
                    return;
                }
                StringRepr::Cons { left, right, .. } => {
                    self.stack.push(NodeFrame::Cons { right });
                    current = left;
                }
                StringRepr::Thin(bytes) => {
                    self.stack.push(NodeFrame::Latin1 { slice: bytes });
                    return;
                }
            }
        }
    }
}

impl<'a> Iterator for CodeUnits<'a> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        loop {
            let last = self.stack.last_mut()?;
            match last {
                NodeFrame::Flat { slice, pos } => {
                    if *pos < slice.len() {
                        let unit = slice[*pos];
                        *pos += 1;
                        return Some(unit);
                    }
                    self.stack.pop();
                }
                NodeFrame::Sliced { slice } => {
                    if !slice.is_empty() {
                        let unit = slice[0];
                        *slice = &slice[1..];
                        return Some(unit);
                    }
                    self.stack.pop();
                }
                NodeFrame::Cons { right } => {
                    let r = *right;
                    self.stack.pop();
                    self.push(r);
                }
                NodeFrame::Latin1 { slice } => {
                    if !slice.is_empty() {
                        let byte = slice[0];
                        *slice = &slice[1..];
                        return Some(u16::from(byte));
                    }
                    self.stack.pop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> StringHeap {
        StringHeap::default()
    }

    #[test]
    fn from_str_roundtrip() {
        let s = JsString::from_str("abc", &h()).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.to_lossy_string(), "abc");
    }

    #[test]
    fn latin1_thin_roundtrip() {
        let heap = h();
        let thin = JsString::from_latin1(b"abc", &heap).unwrap();
        assert_eq!(thin.len(), 3);
        assert_eq!(thin.to_lossy_string(), "abc");
        assert_eq!(thin.char_code_at(0), Some(b'a' as u16));
        assert_eq!(thin.char_code_at(1), Some(b'b' as u16));
        assert_eq!(thin.char_code_at(2), Some(b'c' as u16));
        assert_eq!(thin.char_code_at(3), None);
    }

    #[test]
    fn latin1_equals_flat_for_ascii() {
        let heap = h();
        let thin = JsString::from_latin1(b"hello", &heap).unwrap();
        let flat = JsString::from_str("hello", &heap).unwrap();
        assert!(thin.equals(&flat));
        assert!(flat.equals(&thin));
    }

    #[test]
    fn latin1_slice_stays_thin() {
        let heap = h();
        let thin = JsString::from_latin1(b"abcdef", &heap).unwrap();
        let mid = thin.slice(1, 3, &heap).unwrap();
        assert_eq!(mid.len(), 3);
        assert_eq!(mid.to_lossy_string(), "bcd");
    }

    #[test]
    fn latin1_in_cons_rope_iterates() {
        let heap = h();
        let left = JsString::from_latin1(b"foo", &heap).unwrap();
        let right = JsString::from_str("bar", &heap).unwrap();
        let cons = JsString::concat(&left, &right, &heap).unwrap();
        assert_eq!(cons.to_lossy_string(), "foobar");
        assert_eq!(cons.char_code_at(2), Some(b'o' as u16));
        assert_eq!(cons.char_code_at(3), Some(b'b' as u16));
    }

    #[test]
    fn equality_on_flat() {
        let a = JsString::from_str("abc", &h()).unwrap();
        let b = JsString::from_str("abc", &h()).unwrap();
        let c = JsString::from_str("abd", &h()).unwrap();
        assert!(a.equals(&b));
        assert!(!a.equals(&c));
    }

    #[test]
    fn concat_produces_cons_not_flat() {
        let h = h();
        let a = JsString::from_str("a", &h).unwrap();
        let b = JsString::from_str("b", &h).unwrap();
        let ab = JsString::concat(&a, &b, &h).unwrap();
        assert!(matches!(*ab.repr, StringRepr::Cons { .. }));
        assert_eq!(ab.len(), 2);
        assert_eq!(ab.to_lossy_string(), "ab");
    }

    #[test]
    fn concat_loop_is_linear() {
        // Build s += "abcd" 1000 times. Each step is O(1) cons work.
        let h = h();
        let mut s = JsString::empty(&h).unwrap();
        let piece = JsString::from_str("abcd", &h).unwrap();
        for _ in 0..1_000 {
            s = JsString::concat(&s, &piece, &h).unwrap();
        }
        assert_eq!(s.len(), 4_000);
        assert_eq!(s.to_lossy_string().len(), 4_000);
    }

    #[test]
    fn slice_returns_view_for_flat_parent() {
        let h = h();
        let s = JsString::from_str("abcdef", &h).unwrap();
        let sliced = s.slice(1, 3, &h).unwrap();
        assert_eq!(sliced.len(), 3);
        assert_eq!(sliced.to_lossy_string(), "bcd");
        assert!(matches!(*sliced.repr, StringRepr::Sliced { .. }));
    }

    #[test]
    fn slice_of_slice_collapses() {
        let h = h();
        let s = JsString::from_str("abcdef", &h).unwrap();
        let outer = s.slice(1, 5, &h).unwrap(); // "bcdef"
        let inner = outer.slice(1, 2, &h).unwrap(); // "cd"
        assert_eq!(inner.to_lossy_string(), "cd");
        match &*inner.repr {
            StringRepr::Sliced { start, len, .. } => {
                assert_eq!(*start, 2);
                assert_eq!(*len, 2);
            }
            other => panic!("expected Sliced, got {other:?}"),
        }
    }

    #[test]
    fn slice_of_cons_flattens() {
        let h = h();
        let a = JsString::from_str("ab", &h).unwrap();
        let b = JsString::from_str("cd", &h).unwrap();
        let cons = JsString::concat(&a, &b, &h).unwrap();
        let sliced = cons.slice(1, 2, &h).unwrap();
        assert_eq!(sliced.to_lossy_string(), "bc");
    }

    #[test]
    fn char_code_at_walks_rope() {
        let h = h();
        let a = JsString::from_str("ab", &h).unwrap();
        let b = JsString::from_str("cd", &h).unwrap();
        let cons = JsString::concat(&a, &b, &h).unwrap();
        assert_eq!(cons.char_code_at(0), Some(b'a' as u16));
        assert_eq!(cons.char_code_at(2), Some(b'c' as u16));
        assert_eq!(cons.char_code_at(4), None);
    }

    #[test]
    fn surrogate_round_trip() {
        // Construct from a lone surrogate pair manually.
        let h = h();
        // U+10000 — '𐀀' encoded as a surrogate pair 0xD800 0xDC00.
        let units: [u16; 2] = [0xD800, 0xDC00];
        let s = JsString::from_utf16_units(&units, &h).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.char_code_at(0), Some(0xD800));
        assert_eq!(s.char_code_at(1), Some(0xDC00));
        assert_eq!(s.to_utf16_vec(), units);
    }

    #[test]
    fn flatten_is_iterative_on_deep_rope() {
        let h = h();
        // Build a left-leaning rope of depth ~MAX_ROPE_DEPTH; concat
        // auto-flattens when deeper. Verify length matches.
        let leaf = JsString::from_str("ab", &h).unwrap();
        let mut acc = leaf.clone();
        for _ in 0..(MAX_ROPE_DEPTH * 2) {
            acc = JsString::concat(&acc, &leaf, &h).unwrap();
        }
        let len = acc.len();
        let flat = acc.flatten(&h).unwrap();
        assert_eq!(flat.len(), len);
    }

    #[test]
    fn out_of_memory_does_not_mutate_counter() {
        // Allocate a 4 KiB-cap heap and request a 100-KiB string.
        let h = StringHeap::with_cap(4 * 1024);
        let big_text: String = "a".repeat(100 * 1024);
        let before = h.used();
        let err = JsString::from_str(&big_text, &h).unwrap_err();
        assert!(matches!(err, StringError::OutOfMemory { .. }));
        assert_eq!(
            h.used(),
            before,
            "heap counter must not advance on rejected alloc"
        );
    }

    #[test]
    fn empty_concat_is_identity() {
        let h = h();
        let a = JsString::from_str("abc", &h).unwrap();
        let empty = JsString::empty(&h).unwrap();
        let r1 = JsString::concat(&a, &empty, &h).unwrap();
        let r2 = JsString::concat(&empty, &a, &h).unwrap();
        assert_eq!(r1.to_lossy_string(), "abc");
        assert_eq!(r2.to_lossy_string(), "abc");
    }
}
