//! Interned JavaScript strings
//!
//! Strings are immutable and interned for deduplication.
//! This allows fast equality comparison (pointer comparison).
//!
//! ## GC Integration
//!
//! Strings are managed via `GcRef<JsString>` which wraps a `GcBox<JsString>`.
//! The `GcBox` provides the GC header for marking. Interned strings are kept
//! alive by the intern table (acting as a GC root).
//!
//! ## Per-Isolate String Tables
//!
//! Each `VmRuntime` owns a `StringTable` instance. When a runtime or isolate
//! is active, a thread-local pointer (`THREAD_STRING_TABLE`) is set to the
//! runtime's table. `JsString::intern()` and `intern_utf16()` use this pointer.
//!
//! This follows the same pattern as `THREAD_MEMORY_MANAGER` and `THREAD_REGISTRY`.

use crate::gc::GcRef;
use rustc_hash::{FxHashMap, FxHasher};
use smallvec::SmallVec;
use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

// ============================================================================
// Thread-local state
// ============================================================================

// Per-runtime string table pointer. Set by `VmRuntime::with_config()` during
// construction, `Isolate::enter()` during entry, and cleared by
// `IsolateGuard::drop()` / `VmRuntime::drop()`.
//
// SAFETY: The pointer is valid for the duration of the VmRuntime or Isolate
// guard. It points to the `StringTable` owned by `VmRuntime`, which outlives
// any guard.
thread_local! {
    pub(crate) static THREAD_STRING_TABLE: Cell<*const StringTable> = const { Cell::new(std::ptr::null()) };
}

// ============================================================================
// StringTable — per-runtime string interning
// ============================================================================

/// String interning table for per-runtime/per-isolate management.
///
/// Each `VmRuntime` owns one `StringTable`. When the runtime is active on a
/// thread, `THREAD_STRING_TABLE` points to it so `JsString::intern()` can
/// find the correct table without explicit parameters.
pub struct StringTable {
    strings: RefCell<FxHashMap<u64, SmallVec<[GcRef<JsString>; 1]>>>,
}

// SAFETY: StringTable is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction: each Isolate
// is `Send` but `!Sync`, ensuring single-thread access to the string table.
unsafe impl Send for StringTable {}
unsafe impl Sync for StringTable {}

impl StringTable {
    /// Create a new empty string table.
    pub fn new() -> Self {
        Self {
            strings: RefCell::new(FxHashMap::default()),
        }
    }

    // ---- Thread-local management (same pattern as MemoryManager) ----------

    /// Set the thread-local string table pointer to this table.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` outlives the thread-local reference.
    /// In practice this is guaranteed by `VmRuntime` ownership or `IsolateGuard` RAII.
    pub fn set_thread_default(table: &StringTable) {
        THREAD_STRING_TABLE.with(|cell| cell.set(table as *const StringTable));
    }

    /// Clear the thread-local string table pointer.
    pub fn clear_thread_default() {
        THREAD_STRING_TABLE.with(|cell| cell.set(std::ptr::null()));
    }

    /// Clear the thread-local string table pointer only if it points to `table`.
    ///
    /// Used during teardown to avoid clearing another runtime's table.
    pub fn clear_thread_default_if(table: &StringTable) {
        THREAD_STRING_TABLE.with(|cell| {
            let current = cell.get();
            if std::ptr::eq(current, table) {
                cell.set(std::ptr::null());
            }
        });
    }

    // ---- Interning --------------------------------------------------------

    /// Intern a string in this table.
    pub fn intern(&self, s: &str) -> GcRef<JsString> {
        let hash = JsString::compute_hash_str(s);

        // Check if already interned
        if let Ok(borrowed) = self.strings.try_borrow()
            && let Some(bucket) = borrowed.get(&hash)
        {
            for existing in bucket.iter() {
                if JsString::utf16_equals_str(existing.as_utf16(), s) {
                    Self::perform_read_barrier(*existing);
                    return *existing;
                }
            }
        }

        // Create new interned string
        let bytes = s.as_bytes();
        let utf16_data: Arc<[u16]> = if bytes.iter().all(|&b| b < 0x80) {
            // Fast ASCII path: direct widening, single allocation
            let mut units = Vec::with_capacity(bytes.len());
            for &b in bytes {
                units.push(b as u16);
            }
            Arc::from(units)
        } else {
            Arc::from_iter(s.encode_utf16())
        };
        let js_str = GcRef::new(JsString {
            repr: StringRepr::Flat(utf16_data),
            flattened: OnceLock::new(),
            utf8: OnceLock::new(),
            hash: OnceLock::from(hash),
        });

        // Add to the hash bucket
        if let Ok(mut borrowed) = self.strings.try_borrow_mut() {
            borrowed.entry(hash).or_default().push(js_str);
        }
        js_str
    }

    /// Intern a UTF-16 string in this table.
    pub fn intern_utf16(&self, units: &[u16]) -> GcRef<JsString> {
        let hash = JsString::compute_hash_units(units);

        // Check if already interned
        if let Ok(borrowed) = self.strings.try_borrow()
            && let Some(bucket) = borrowed.get(&hash)
        {
            for existing in bucket.iter() {
                if existing.as_utf16() == units {
                    Self::perform_read_barrier(*existing);
                    return *existing;
                }
            }
        }

        let js_str = GcRef::new(JsString {
            repr: StringRepr::Flat(Arc::from(units)),
            flattened: OnceLock::new(),
            utf8: OnceLock::new(),
            hash: OnceLock::from(hash),
        });

        // Add to the hash bucket
        if let Ok(mut borrowed) = self.strings.try_borrow_mut() {
            borrowed.entry(hash).or_default().push(js_str);
        }
        js_str
    }

    /// Check if a string is interned in this table.
    pub fn is_interned(&self, s: &str) -> bool {
        let hash = JsString::compute_hash_str(s);
        let borrowed = self.strings.borrow();
        if let Some(bucket) = borrowed.get(&hash) {
            for existing in bucket.iter() {
                if JsString::utf16_equals_str(existing.as_utf16(), s) {
                    return true;
                }
            }
        }
        false
    }

    /// Perform a read barrier when an interned string is retrieved from the table.
    ///
    /// If an incremental GC is currently marking, and the retrieved string is
    /// White (unmarked), we must mark it Gray to prevent it from being swept.
    /// Weak cache retrievals resurrect references that the GC might have already
    /// missed if the string had no active roots earlier in the GC cycle.
    #[inline]
    fn perform_read_barrier(js_str: GcRef<JsString>) {
        let registry = otter_vm_gc::global_registry();
        if !registry.is_marking() {
            return;
        }
        let header_ptr = js_str.header() as *const otter_vm_gc::GcHeader;
        let header = unsafe { &*header_ptr };
        use otter_vm_gc::object::MarkColor;
        if header.mark() == MarkColor::White {
            header.set_mark(MarkColor::Gray);
            otter_vm_gc::barrier_push(header_ptr);
        }
    }

    /// Get the number of hash buckets in the table.
    pub fn len(&self) -> usize {
        self.strings.borrow().len()
    }

    /// Check if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.strings.borrow().is_empty()
    }

    // ---- GC integration ---------------------------------------------------

    /// Remove all entries from the table.
    ///
    /// Called during runtime teardown to prevent dangling `GcRef<JsString>`
    /// after `dealloc_all()` frees the underlying GC objects.
    pub fn clear(&self) {
        self.strings.borrow_mut().clear();
    }

    /// Prune dead (unmarked) entries from this table.
    ///
    /// **Must be called after the mark phase and before the sweep phase** of a
    /// full GC cycle. Entries whose mark color is `White` were not reached from
    /// any GC root and will be freed by the upcoming sweep. Removing them here
    /// prevents dangling `GcRef`s.
    pub fn prune_dead_entries(&self) {
        use otter_vm_gc::object::MarkColor;
        let mut borrowed = self.strings.borrow_mut();
        for bucket in borrowed.values_mut() {
            // SAFETY: called after mark, before sweep — all GcBox memory is still
            // valid. Objects with mark() == White will be freed by sweep.
            bucket.retain(|js_str| js_str.header().mark() != MarkColor::White);
        }
        // Remove hash buckets that became empty after pruning.
        borrowed.retain(|_, bucket| !bucket.is_empty());
    }

    /// Trace all interned strings in this table.
    ///
    /// Called during GC root collection to keep ALL interned strings alive.
    /// **Only use this when the weak-ref eviction path (`prune_dead_entries`)
    /// is not in effect.** The two approaches are mutually exclusive.
    pub fn trace_all(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // GC mark phase does not allocate or intern strings, so holding the
        // borrow for the duration is safe (no re-entrance into string table).
        let borrowed = self.strings.borrow();
        for bucket in borrowed.values() {
            for js_str in bucket.iter() {
                tracer(js_str.header() as *const _);
            }
        }
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Free functions — operate on the current thread's string table
// ============================================================================

/// Clear the current thread's string intern table.
///
/// Clears the per-runtime table pointed to by `THREAD_STRING_TABLE`.
///
/// Call this when tearing down a VM to allow the GC to reclaim interned string
/// memory. After calling this, all existing `GcRef<JsString>` from the table
/// are dangling — only use this when no VM is active on this thread.
pub fn clear_global_string_table() {
    THREAD_STRING_TABLE.with(|cell| {
        let ptr = cell.get();
        if !ptr.is_null() {
            // SAFETY: pointer is valid — set by VmRuntime/Isolate, cleared on exit.
            let table = unsafe { &*ptr };
            table.clear();
        }
    });
}

/// Get the number of entries in the current thread's string intern table.
pub fn global_string_table_size() -> usize {
    THREAD_STRING_TABLE.with(|cell| {
        let ptr = cell.get();
        if !ptr.is_null() {
            let table = unsafe { &*ptr };
            return table.len();
        }
        0
    })
}

// ============================================================================
// JsString
// ============================================================================

/// Internal representation of a JavaScript string
#[derive(Clone)]
pub enum StringRepr {
    /// A flat string backed by a contiguous UTF-16 buffer
    Flat(Arc<[u16]>),
    /// A rope string representing the concatenation of two other strings
    Rope {
        left: GcRef<JsString>,
        right: GcRef<JsString>,
        len: usize,
        depth: u16,
    },
}

/// An interned or rope JavaScript string with GC support
///
/// `JsString` is allocated via `GcRef<JsString>` which wraps it in a `GcBox`.
/// The `GcBox` provides the GC header for marking.
#[derive(Clone)]
pub struct JsString {
    /// Internal representation (flat or rope)
    repr: StringRepr,
    /// Cached flattened UTF-16 representation (for Ropes)
    flattened: OnceLock<Arc<[u16]>>,
    /// Cached UTF-8 representation (lossy for lone surrogates)
    utf8: OnceLock<Arc<str>>,
    /// Precomputed or lazily-computed hash for fast lookup
    hash: OnceLock<u64>,
}

impl JsString {
    /// Ensure this string and its children (if it's a rope) are tenured.
    ///
    /// Tenuring an object prevents it from being swept during minor GC cycles,
    /// which is necessary when a non-GC object (like a Shape transition)
    /// holds a reference to it.
    pub fn ensure_tenured(&self) {
        if let StringRepr::Rope { left, right, .. } = &self.repr {
            left.header().set_tenured();
            right.header().set_tenured();
            // We recursively tenure children to ensure the whole rope survives.
            left.ensure_tenured();
            right.ensure_tenured();
        }
    }

    /// Create or retrieve an interned string.
    ///
    /// Uses the per-runtime `StringTable` via `THREAD_STRING_TABLE`.
    ///
    /// # Panics
    ///
    /// Panics if no `StringTable` is set on the current thread (i.e. no
    /// `VmRuntime` or `Isolate` is active).
    pub fn intern(s: &str) -> GcRef<Self> {
        let table_ptr = THREAD_STRING_TABLE.with(|cell| cell.get());
        assert!(
            !table_ptr.is_null(),
            "JsString::intern() called without an active VmRuntime or Isolate on this thread"
        );
        // SAFETY: pointer is valid for the duration of the VmRuntime/Isolate.
        let table = unsafe { &*table_ptr };
        table.intern(s)
    }

    /// Create a string without interning (for temporary strings)
    ///
    /// Returns a `GcRef<JsString>` for the new string.
    /// Pre-caches the UTF-8 representation since we already have it.
    pub fn new_gc(s: impl AsRef<str>) -> GcRef<Self> {
        let s_ref = s.as_ref();
        let bytes = s_ref.as_bytes();
        // Fast path for ASCII: direct widening without encode_utf16 iterator overhead
        let units: Vec<u16> = if bytes.iter().all(|&b| b < 0x80) {
            let mut v = Vec::with_capacity(bytes.len());
            for &b in bytes {
                v.push(b as u16);
            }
            v
        } else {
            s_ref.encode_utf16().collect()
        };
        // Pre-cache the UTF-8 form since we already have it as a Rust &str
        let utf8_cache = OnceLock::new();
        let _ = utf8_cache.set(Arc::<str>::from(s_ref));
        GcRef::new(Self {
            repr: StringRepr::Flat(units.into()),
            flattened: OnceLock::new(),
            utf8: utf8_cache,
            hash: OnceLock::new(),
        })
    }

    /// Create a string from UTF-16 code units without interning
    pub fn from_utf16_units(units: Vec<u16>) -> Self {
        Self {
            repr: StringRepr::Flat(units.into()),
            flattened: OnceLock::new(),
            utf8: OnceLock::new(),
            hash: OnceLock::new(),
        }
    }

    /// Create a GcRef<JsString> from UTF-16 code units without interning
    pub fn from_utf16_units_gc(units: Vec<u16>) -> GcRef<Self> {
        GcRef::new(Self::from_utf16_units(units))
    }

    /// Create or retrieve an interned string from UTF-16 code units.
    ///
    /// Uses the per-runtime `StringTable` via `THREAD_STRING_TABLE`.
    ///
    /// # Panics
    ///
    /// Panics if no `StringTable` is set on the current thread.
    pub fn intern_utf16(units: &[u16]) -> GcRef<Self> {
        let table_ptr = THREAD_STRING_TABLE.with(|cell| cell.get());
        assert!(
            !table_ptr.is_null(),
            "JsString::intern_utf16() called without an active VmRuntime or Isolate on this thread"
        );
        // SAFETY: pointer is valid for the duration of the VmRuntime/Isolate.
        let table = unsafe { &*table_ptr };
        table.intern_utf16(units)
    }

    /// Get the string as a str slice
    #[inline]
    pub fn as_str(&self) -> &str {
        let cached = self.utf8.get_or_init(|| {
            let s = String::from_utf16_lossy(self.as_utf16());
            Arc::<str>::from(s)
        });
        cached.as_ref()
    }

    /// Get the UTF-16 code units
    ///
    /// This may trigger flattening if the string is currently a rope.
    /// Note: Interior mutability is handled by the GcRef wrapper + GcBox
    /// but since we want to return a slice to the interior Arc buffer,
    /// we must ensure the rope is flattened into a Flat representation.
    ///
    /// SAFETY: This method effectively performs lazy flattening.
    /// To return a `&[u16]`, we return the slice from the `Flat` variant's `Arc`.
    /// If it's a `Rope`, we MUST flatten it.
    #[inline]
    pub fn as_utf16(&self) -> &[u16] {
        match &self.repr {
            StringRepr::Flat(data) => data,
            StringRepr::Rope { .. } => self.flattened.get_or_init(|| {
                let mut units = Vec::with_capacity(self.len_utf16());
                self.write_utf16_to(&mut units);
                Arc::from(units)
            }),
        }
    }

    fn write_utf16_to(&self, buf: &mut Vec<u16>) {
        let mut worklist = vec![self];
        while let Some(current) = worklist.pop() {
            // Check if we have a flattened version first (cached)
            if let Some(data) = current.flattened.get() {
                buf.extend_from_slice(data);
                continue;
            }

            match &current.repr {
                StringRepr::Flat(data) => buf.extend_from_slice(data),
                StringRepr::Rope { left, right, .. } => {
                    // Push right then left so left is processed first (LIFO)
                    worklist.push(&**right);
                    worklist.push(&**left);
                }
            }
        }
    }

    /// Get the length in UTF-16 code units (for JS compatibility)
    pub fn len_utf16(&self) -> usize {
        match &self.repr {
            StringRepr::Flat(data) => data.len(),
            StringRepr::Rope { len, .. } => *len,
        }
    }

    /// Get the length in bytes
    #[inline]
    pub fn len(&self) -> usize {
        self.len_utf16()
    }

    /// Check if string is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len_utf16() == 0
    }

    /// Get precomputed hash value
    #[inline]
    pub fn hash_value(&self) -> u64 {
        *self.hash.get_or_init(|| {
            match &self.repr {
                StringRepr::Flat(data) => Self::compute_hash_units(data),
                StringRepr::Rope { .. } => {
                    // For ropes, we flatten to compute the hash accurately
                    // (alternatively we could combine hashes but that's risky)
                    Self::compute_hash_units(self.as_utf16())
                }
            }
        })
    }

    /// Concatenate two strings
    ///
    /// For small strings or small total length, we may still intern.
    /// But for larger ones, we create a Rope.
    pub fn concat(&self, other: &JsString) -> GcRef<Self> {
        let left_len = self.len_utf16();
        let right_len = other.len_utf16();
        let total_len = left_len + right_len;

        // Optimization: if both are small, just flatten and intern immediately
        if total_len < 64 {
            let mut units = Vec::with_capacity(total_len);
            self.write_utf16_to(&mut units);
            other.write_utf16_to(&mut units);
            return Self::intern_utf16(&units);
        }

        // Create a Rope
        // We need self and other to be GcRef. But they are passed as &JsString.
        // This is a problem because we need to store them in the Rope.
        // Actually, in the interpreter, we usually have GcRef<JsString>.
        // Let's change the signature or provide a way to get GcRef if we are in a GcBox.
        // For now, let's assume we can get the GcRef from the caller or re-allocate.
        // Wait, if we are in JsString::concat(&self, other), we don't know our own GcRef.
        // This means we should probably implement this at the GcRef level or Value level.

        // Let's implement a version that takes GcRefs.
        unreachable!("Use JsString::concat_gc")
    }

    /// Concatenate two GcRef strings, potentially creating a rope.
    pub fn concat_gc(left: GcRef<Self>, right: GcRef<Self>) -> GcRef<Self> {
        let left_len = left.len_utf16();
        let right_len = right.len_utf16();
        let total_len = left_len + right_len;

        // If total length is small, intern to avoid rope overhead
        if total_len < 64 {
            if total_len == 0 {
                return left;
            }
            if left_len == 0 {
                return right;
            }
            if right_len == 0 {
                return left;
            }

            let mut units = Vec::with_capacity(total_len);
            left.write_utf16_to(&mut units);
            right.write_utf16_to(&mut units);
            return Self::intern_utf16(&units);
        }

        let left_depth = match &left.repr {
            StringRepr::Flat(_) => 0,
            StringRepr::Rope { depth, .. } => *depth,
        };
        let right_depth = match &right.repr {
            StringRepr::Flat(_) => 0,
            StringRepr::Rope { depth, .. } => *depth,
        };

        let depth = left_depth.max(right_depth) + 1u16;

        // If depth is too high, flatten to avoid extreme recursion or tree imbalance.
        // We use an iterative write_utf16_to, so 1000 is safe from stack overflow.
        if depth > 1000 {
            // eprintln!("ROPE DEPTH LIMIT REACHED: flattening string of len {}", total_len);
            let mut units = Vec::with_capacity(total_len);
            left.write_utf16_to(&mut units);
            right.write_utf16_to(&mut units);

            // Only intern if the string is reasonably small
            if total_len < 256 {
                return Self::intern_utf16(&units);
            } else {
                return Self::from_utf16_units_gc(units);
            }
        }

        GcRef::new(Self {
            repr: StringRepr::Rope {
                left,
                right,
                len: total_len,
                depth,
            },
            flattened: OnceLock::new(),
            utf8: OnceLock::new(),
            hash: OnceLock::new(),
        })
    }

    /// Get character at index (UTF-16 code unit)
    pub fn char_at(&self, index: usize) -> Option<char> {
        self.as_utf16()
            .get(index)
            .and_then(|unit| char::from_u32(*unit as u32))
    }

    /// Get substring (character-based)
    pub fn substring(&self, start: usize, end: usize) -> GcRef<Self> {
        let s = self.as_str();
        let chars: Vec<char> = s.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let result: String = chars[start..end].iter().collect();
        Self::intern(&result)
    }

    /// Get substring with UTF-16 semantics (for JS String.prototype.substring)
    ///
    /// JavaScript strings use UTF-16 internally, so indices are in UTF-16 code units.
    pub fn substring_utf16(&self, start: usize, end: usize) -> GcRef<Self> {
        let units = self.as_utf16();
        let start = start.min(units.len());
        let end = end.min(units.len()).max(start);
        let slice = &units[start..end];
        Self::intern_utf16(slice)
    }

    /// Concatenate using a specific string table instead of thread-local intern
    pub fn concat_with_table(&self, other: &JsString, table: &StringTable) -> GcRef<Self> {
        let mut units = Vec::with_capacity(self.len_utf16() + other.len_utf16());
        self.write_utf16_to(&mut units);
        other.write_utf16_to(&mut units);
        table.intern_utf16(&units)
    }

    fn compute_hash_units(units: &[u16]) -> u64 {
        Self::compute_hash_utf16_iter(units.iter().copied())
    }

    fn compute_hash_str(s: &str) -> u64 {
        Self::compute_hash_utf16_iter(s.encode_utf16())
    }

    fn compute_hash_utf16_iter<I>(units: I) -> u64
    where
        I: Iterator<Item = u16>,
    {
        let mut hasher = FxHasher::default();
        let mut len = 0usize;
        for unit in units {
            unit.hash(&mut hasher);
            len += 1;
        }
        len.hash(&mut hasher);
        hasher.finish()
    }

    fn utf16_equals_str(units: &[u16], s: &str) -> bool {
        let mut idx = 0usize;
        for unit in s.encode_utf16() {
            if idx >= units.len() || units[idx] != unit {
                return false;
            }
            idx += 1;
        }
        idx == units.len()
    }
}

impl std::fmt::Debug for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsString({:?})", self.as_str())
    }
}

impl std::fmt::Display for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        // Debug guard: detect near-null/freed pointer access.
        // The crash at address 0x58 means self or other is ~0x8, which happens
        // when a freed GcBox is accessed (first word = freelist ptr).
        let self_ptr = self as *const Self as usize;
        let other_ptr = other as *const Self as usize;
        if self_ptr < 0x1000 || other_ptr < 0x1000 {
            let valid_key = if other_ptr >= 0x1000 {
                other.as_str()
            } else if self_ptr >= 0x1000 {
                self.as_str()
            } else {
                "<both invalid>"
            };
            panic!(
                "JsString::eq called with a freed/invalid pointer: self={:#x} other={:#x} (valid key='{}')",
                self_ptr, other_ptr, valid_key
            );
        }
        // Fast path: same hash means likely same string
        if self.hash_value() != other.hash_value() {
            return false;
        }
        // Verify with actual comparison
        if self.len_utf16() != other.len_utf16() {
            return false;
        }
        self.as_utf16() == other.as_utf16()
    }
}

impl Eq for JsString {}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash_value().hash(state);
    }
}

impl AsRef<str> for JsString {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u16]> for JsString {
    fn as_ref(&self) -> &[u16] {
        self.as_utf16()
    }
}

// Note: JsString no longer implements GcObject directly.
// The GcBox<JsString> wrapper (via GcRef) provides the GC header.
// JsString::trace is a no-op since strings don't contain GC references.

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl otter_vm_gc::GcTraceable for JsString {
    // Strings contain GC references if they are Ropes
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::STRING;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        if let StringRepr::Rope { left, right, .. } = &self.repr {
            tracer(left.header() as *const otter_vm_gc::GcHeader);
            tracer(right.header() as *const otter_vm_gc::GcHeader);
        }
    }
}

/// Trace all interned strings in the current thread's string table.
///
/// **Only use this when the weak-ref eviction path (`prune_dead_string_table_entries`)
/// is not in effect.**  The two approaches are mutually exclusive: calling this
/// re-roots all strings and defeats the weak-ref eviction.
pub fn trace_global_string_table(tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
    THREAD_STRING_TABLE.with(|cell| {
        let ptr = cell.get();
        if !ptr.is_null() {
            // SAFETY: pointer is valid — set by VmRuntime/Isolate, cleared on exit.
            let table = unsafe { &*ptr };
            table.trace_all(tracer);
        }
    });
}

/// Prune dead entries from the current thread's string table.
///
/// **Must be called after the mark phase and before the sweep phase** of a
/// full GC cycle.  At that point every `GcBox<JsString>` is still in memory
/// (sweep has not run yet), so reading the GC header is safe.  Entries whose
/// mark color is `White` were not reached from any GC root — they will be
/// freed by the upcoming sweep, so they must be removed from the table now to
/// prevent dangling `GcRef`s.
///
/// Callers that use this pruning approach MUST NOT also call
/// `trace_global_string_table()` for the same GC cycle — doing so would
/// re-root all strings and defeat eviction.
pub fn prune_dead_string_table_entries() {
    THREAD_STRING_TABLE.with(|cell| {
        let ptr = cell.get();
        if !ptr.is_null() {
            // SAFETY: pointer is valid — set by VmRuntime/Isolate, cleared on exit.
            let table = unsafe { &*ptr };
            table.prune_dead_entries();
        }
    });
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interning() {
        let _rt = crate::runtime::VmRuntime::new();
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern("hello");

        // Should be the same GcRef (interned) - same pointer
        assert_eq!(s1.as_ptr(), s2.as_ptr());
    }

    #[test]
    fn test_different_strings() {
        let _rt = crate::runtime::VmRuntime::new();
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern("world");

        assert_ne!(s1.as_ptr(), s2.as_ptr());
        assert_ne!(s1.hash_value(), s2.hash_value());
    }

    #[test]
    fn test_concat() {
        let _rt = crate::runtime::VmRuntime::new();
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern(" world");
        let result = s1.concat(&s2);

        assert_eq!(result.as_str(), "hello world");
    }

    #[test]
    fn test_substring() {
        let _rt = crate::runtime::VmRuntime::new();
        let s = JsString::intern("hello world");
        let sub = s.substring(0, 5);

        assert_eq!(sub.as_str(), "hello");
    }

    #[test]
    fn test_string_table() {
        let _rt = crate::runtime::VmRuntime::new();
        let table = StringTable::new();

        let s1 = table.intern("hello");
        let s2 = table.intern("hello");
        let s3 = table.intern("world");

        // Same string should return same GcRef (same pointer)
        assert_eq!(s1.as_ptr(), s2.as_ptr());
        // Different string should be different
        assert_ne!(s1.as_ptr(), s3.as_ptr());

        assert!(table.is_interned("hello"));
        assert!(table.is_interned("world"));
        assert!(!table.is_interned("foo"));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_substring_utf16() {
        let _rt = crate::runtime::VmRuntime::new();
        let s = JsString::intern("hello world");
        let sub = s.substring_utf16(0, 5);
        assert_eq!(sub.as_str(), "hello");

        // Test with end > start
        let sub2 = s.substring_utf16(6, 11);
        assert_eq!(sub2.as_str(), "world");
    }

    #[test]
    fn test_substring_utf16_emoji() {
        let _rt = crate::runtime::VmRuntime::new();
        // Emoji (like 😀) is represented as a surrogate pair in UTF-16
        let s = JsString::intern("a😀b");

        // UTF-16: 'a' (1), surrogate pair for 😀 (2), 'b' (1) = 4 code units
        assert_eq!(s.len_utf16(), 4);

        // Get just 'a'
        let sub = s.substring_utf16(0, 1);
        assert_eq!(sub.as_str(), "a");

        // Get the emoji (needs both surrogate pair code units)
        let sub_emoji = s.substring_utf16(1, 3);
        assert_eq!(sub_emoji.as_str(), "😀");
    }

    #[test]
    fn test_gcref_header() {
        let _rt = crate::runtime::VmRuntime::new();
        use otter_vm_gc::object::MarkColor;

        let s = JsString::intern("test");

        // GcRef provides header via GcBox
        let header = s.header();

        // Default mark should be white
        assert_eq!(header.mark(), MarkColor::White);
    }

    #[test]
    fn test_concat_with_table() {
        let _rt = crate::runtime::VmRuntime::new();
        let table = StringTable::new();
        let s1 = table.intern("hello");
        let s2 = table.intern(" world");

        let result = s1.concat_with_table(&s2, &table);
        assert_eq!(result.as_str(), "hello world");

        // Should be interned in the table
        assert!(table.is_interned("hello world"));
    }

    #[test]
    fn test_string_table_clear() {
        let _rt = crate::runtime::VmRuntime::new();
        let table = StringTable::new();
        table.intern("foo");
        table.intern("bar");
        assert_eq!(table.len(), 2);

        table.clear();
        assert_eq!(table.len(), 0);
        assert!(table.is_empty());
    }

    #[test]
    fn test_intern_hash_matches_utf16_path() {
        let _rt = crate::runtime::VmRuntime::new();
        let table = StringTable::new();
        let text = "json_key_😀";
        let utf16: Vec<u16> = text.encode_utf16().collect();

        let via_str = table.intern(text);
        let via_utf16 = table.intern_utf16(&utf16);

        assert_eq!(via_str.as_ptr(), via_utf16.as_ptr());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn test_runtime_sets_thread_string_table() {
        // VmRuntime::new() should set THREAD_STRING_TABLE, so JsString::intern works
        let _rt = crate::runtime::VmRuntime::new();
        let s = JsString::intern("runtime_test");
        assert_eq!(s.as_str(), "runtime_test");
    }

    #[test]
    #[should_panic(expected = "without an active VmRuntime")]
    fn test_intern_without_runtime_panics() {
        // Clear any existing thread-local (might be set by other tests)
        StringTable::clear_thread_default();
        // This should panic — no VmRuntime/Isolate active
        let _s = JsString::intern("should_panic");
    }
}
