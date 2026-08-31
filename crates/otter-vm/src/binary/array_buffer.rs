//! JavaScript `ArrayBuffer` and `SharedArrayBuffer` (ECMA-262 §25.1
//! / §25.2).
//!
//! Storage lives on the GC heap. The Local path stores the byte
//! buffer inline in [`LocalArrayBufferBodyGc`]; mutators flip every
//! field through [`otter_gc::GcHeap::with_payload`]. The Shared path
//! wraps an `Arc<SharedBody>` (cross-thread, `Mutex<Vec<u8>>`-guarded)
//! inside [`SharedArrayBufferBodyGc`] so the same buffer can hop
//! across isolates while staying accounted by each one's GC budget.
//!
//! # Contents
//! - [`JsArrayBuffer`] — `Copy`/`Eq`/`Hash` handle. Variant tag in
//!   [`BufferStorage`] selects between Local and Shared GC bodies.
//! - [`LocalArrayBufferBodyGc`] / [`SharedArrayBufferBodyGc`] — GC
//!   bodies.
//! - [`SharedBody`] — cross-thread substrate behind `Arc`. Keeps a
//!   process-unique id used by [`crate::atomics_wait`].
//!
//! # Invariants
//! - `Local`: when `detached == true`, the byte buffer is empty.
//!   Every operation that needs the bytes must check
//!   [`JsArrayBuffer::is_detached`] first per §25.1.3.1
//!   `IsDetachedBuffer`.
//! - `Shared`: never detached (§25.2.4.1 step 2). Only growth via
//!   [`JsArrayBuffer::grow`] is allowed.
//! - For resizable / growable buffers, `max_byte_length` is
//!   `Some(n)` and the underlying `Vec<u8>` capacity is at least
//!   `n`.
//! - [`SharedBody::id`] is monotonically allocated from a static
//!   `AtomicU64` and stays stable for the buffer's lifetime; the
//!   Atomics wait registry keys on `(id, byte_index)`.
//! - No `Rc` / `Arc` / `Cell` / `RefCell` inside GC bodies. Mutators
//!   flip fields via `heap.with_payload`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-sharedarraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-isdetachedbuffer>

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Process-wide monotonic id allocator for `SharedArrayBuffer`. The
/// id is the registry key used by [`crate::atomics_wait`] to map
/// `(buffer, byte_index)` to parked threads.
static NEXT_SHARED_ID: AtomicU64 = AtomicU64::new(1);

fn allocate_shared_id() -> u64 {
    NEXT_SHARED_ID.fetch_add(1, Ordering::Relaxed)
}

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`LocalArrayBufferBodyGc`].
pub const LOCAL_ARRAY_BUFFER_BODY_TYPE_TAG: u8 = 0x2c;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for
/// [`SharedArrayBufferBodyGc`].
pub const SHARED_ARRAY_BUFFER_BODY_TYPE_TAG: u8 = 0x2d;

/// GC body for non-shared `ArrayBuffer` per ECMA-262 §25.1.
///
/// Mutators flip every field through [`otter_gc::GcHeap::with_payload`]
/// (no interior mutability in GC bodies).
///
/// `#[repr(C)]` so the cached `(data, byte_len)` pair and the `detached`
/// flag sit at fixed byte offsets
/// ([`LOCAL_ARRAY_BUFFER_BODY_DATA_OFFSET`] /
/// [`LOCAL_ARRAY_BUFFER_BODY_BYTE_LEN_OFFSET`] /
/// [`LOCAL_ARRAY_BUFFER_BODY_DETACHED_OFFSET`]) for inline typed-array
/// element access — no interpreter round-trip.
#[derive(Debug)]
#[repr(C)]
pub struct LocalArrayBufferBodyGc {
    /// Raw bytes. Empty when detached.
    ///
    /// This Rust container is deliberately not part of any native
    /// contract: `Vec`'s field order is unspecified, so compiled code
    /// reads the cached [`Self::data`] / [`Self::byte_len`] pair
    /// instead. Every path that can move or resize the allocation must
    /// call [`Self::refresh_byte_cache`].
    bytes: Vec<u8>,
    /// Always-current base of the byte storage, kept in a fixed body
    /// field so compiled code can address bytes without knowing `Vec`
    /// layout. The storage is a plain Rust allocation, so a moving
    /// collection of the body leaves it valid; only growth, shrinkage,
    /// reallocation and detach change it. Null for empty or detached
    /// storage, which no access can reach because every access proves
    /// `index < byte_len` first.
    data: std::cell::Cell<*mut u8>,
    /// Always-current `bytes.len()`, mirrored next to the base pointer
    /// so a compiled bounds check is one load.
    byte_len: std::cell::Cell<usize>,
    /// `true` after detach / transfer; once set, stays set per spec.
    pub detached: bool,
    /// `Some(n)` for a resizable buffer; `None` for a fixed-length
    /// buffer.
    pub max_byte_length: Option<usize>,
    /// GC-budget reservation for the off-heap byte storage.
    pub external: Option<otter_gc::ExternalMemory>,
    /// Subclass `[[Prototype]]` selected at construction
    /// (`GetPrototypeFromConstructor`); `None` means the realm's
    /// %ArrayBuffer.prototype%. Lives in the body so a moving
    /// collection carries it with the object — an address-keyed side
    /// table would attach the override to whatever object later
    /// reuses the address.
    pub prototype_override: Option<crate::Value>,
    /// Lazy expando bag for ordinary own properties
    /// (`ab.constructor = C` for the species protocol, `ab.x = 1`).
    pub expando: Option<crate::object::JsObject>,
}

/// Byte offset of the cached byte-storage base within
/// [`LocalArrayBufferBodyGc`]. Compiled element access loads the base
/// from here; the `Vec` itself is never addressed by native code.
pub const LOCAL_ARRAY_BUFFER_BODY_DATA_OFFSET: usize =
    std::mem::offset_of!(LocalArrayBufferBodyGc, data);
/// Byte offset of the cached byte length within
/// [`LocalArrayBufferBodyGc`], read by a compiled bounds check.
pub const LOCAL_ARRAY_BUFFER_BODY_BYTE_LEN_OFFSET: usize =
    std::mem::offset_of!(LocalArrayBufferBodyGc, byte_len);
/// Byte offset of [`LocalArrayBufferBodyGc::detached`] from the body
/// start. The JIT bails inline access when this flag is set.
pub const LOCAL_ARRAY_BUFFER_BODY_DETACHED_OFFSET: usize =
    std::mem::offset_of!(LocalArrayBufferBodyGc, detached);

impl LocalArrayBufferBodyGc {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    /// Recompute the compiled-code-visible byte cache after any mutation
    /// that can move or resize the storage.
    #[inline]
    pub(crate) fn refresh_byte_cache(&self) {
        self.data.set(if self.bytes.is_empty() {
            std::ptr::null_mut()
        } else {
            self.bytes.as_ptr().cast_mut()
        });
        self.byte_len.set(self.bytes.len());
    }

    /// Read-only view of the storage.
    #[inline]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Mutable access to the storage that refreshes the cache afterwards,
    /// so no caller can leave a stale base behind.
    #[inline]
    pub(crate) fn with_bytes_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Vec<u8>) -> R,
    {
        let result = f(&mut self.bytes);
        self.refresh_byte_cache();
        result
    }

    /// Verifier for the always-current byte cache: the cached pair must
    /// equal what [`Self::refresh_byte_cache`] would recompute now, so any
    /// new mutation path that forgets to refresh fails deterministically.
    /// Read paths assert it under `debug_assertions`.
    #[must_use]
    pub(crate) fn byte_cache_is_current(&self) -> bool {
        if self.bytes.is_empty() {
            return self.data.get().is_null() && self.byte_len.get() == 0;
        }
        self.data.get() == self.bytes.as_ptr().cast_mut() && self.byte_len.get() == self.bytes.len()
    }
}

impl otter_gc::SafeTraceable for LocalArrayBufferBodyGc {
    const TYPE_TAG: u8 = LOCAL_ARRAY_BUFFER_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if let Some(expando) = &mut self.expando {
            let p = expando as *mut crate::object::JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if let Some(proto) = &self.prototype_override {
            proto.trace_value_slots(visitor);
        }
        // No outgoing GC slots — the byte storage and its cached base are
        // plain data.
    }
}

/// 4-byte compressed GC handle to a [`LocalArrayBufferBodyGc`].
/// `Copy`. Packs into [`crate::Value`] under `TAG_PTR_OBJECT`.
pub type LocalArrayBufferHandle = otter_gc::Gc<LocalArrayBufferBodyGc>;

/// Allocate a Local `ArrayBuffer` body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_local_array_buffer(
    heap: &mut otter_gc::GcHeap,
    bytes: Vec<u8>,
    max_byte_length: Option<usize>,
    external: Option<otter_gc::ExternalMemory>,
) -> Result<LocalArrayBufferHandle, otter_gc::OutOfMemory> {
    let handle = heap.alloc_old(LocalArrayBufferBodyGc {
        bytes,
        data: std::cell::Cell::new(std::ptr::null_mut()),
        byte_len: std::cell::Cell::new(0),
        detached: false,
        max_byte_length,
        external,
        expando: None,
        prototype_override: None,
    })?;
    // The `Vec`'s buffer is a separate allocation, so moving the body — here
    // and in any later collection — leaves the cached base valid.
    heap.with_payload(handle, |body| body.refresh_byte_cache());
    Ok(handle)
}

/// GC body for `SharedArrayBuffer` per ECMA-262 §25.2.
///
/// The bytes stay outside the GC cage because Atomics ops cross
/// host threads (`Mutex<Vec<u8>>`). The body owns the
/// `Arc<SharedBody>` as plain Rust data.
#[derive(Debug)]
pub struct SharedArrayBufferBodyGc {
    /// Shared backing store. `Arc` survives the GC body's lifetime.
    pub inner: Arc<SharedBody>,
    /// Lazy expando bag for ordinary own properties (`sab.constructor
    /// = C` for the species protocol, `sab.x = 1`). Per-isolate — own
    /// properties are a local view, never shipped across the `Arc`.
    pub expando: Option<crate::object::JsObject>,
    /// Subclass `[[Prototype]]` stamped at construction. Per-isolate,
    /// like the expando: the prototype is a local view, never shipped
    /// across the `Arc`.
    pub prototype_override: Option<crate::Value>,
}

impl otter_gc::SafeTraceable for SharedArrayBufferBodyGc {
    const TYPE_TAG: u8 = SHARED_ARRAY_BUFFER_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        if let Some(expando) = &mut self.expando {
            let p = expando as *mut crate::object::JsObject as *mut otter_gc::raw::RawGc;
            visitor(p);
        }
        if let Some(proto) = &mut self.prototype_override {
            proto.trace_value_slot_mut(visitor);
        }
        // Bytes live behind an `Arc` outside the cage.
    }
}

impl SharedArrayBufferBodyGc {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        if let Some(value) = &self.prototype_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

/// 4-byte compressed GC handle to a [`SharedArrayBufferBodyGc`].
/// `Copy`. Packs into [`crate::Value`] under `TAG_PTR_OBJECT`.
pub type SharedArrayBufferHandle = otter_gc::Gc<SharedArrayBufferBodyGc>;

/// Allocate a Shared `ArrayBuffer` body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_shared_array_buffer(
    heap: &mut otter_gc::GcHeap,
    inner: Arc<SharedBody>,
) -> Result<SharedArrayBufferHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(SharedArrayBufferBodyGc {
        inner,
        expando: None,
        prototype_override: None,
    })
}

/// Cheap-to-copy `ArrayBuffer` / `SharedArrayBuffer` handle.
///
/// Backed by a tagged pair of 4-byte GC handles; `Copy`/`Eq`/`Hash`.
/// `#[repr(transparent)]` so the embedded [`BufferStorage`] sits at
/// offset 0 — the baseline JIT reads the storage discriminant + handle
/// through a `TypedArrayBodyGc.buffer` field at fixed offsets.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct JsArrayBuffer {
    storage: BufferStorage,
}

/// Storage discriminator. `Local` is the single-isolate
/// `ArrayBuffer` path; `Shared` is the cross-thread
/// `SharedArrayBuffer` path.
///
/// `#[repr(C)]` forces a stable `{ tag: u32, handle: u32 }`-shaped
/// layout (8 bytes) so the baseline JIT can read the discriminant and,
/// for the `Local` variant, the compressed buffer handle at fixed byte
/// offsets ([`BUFFER_STORAGE_DISCRIMINANT_OFFSET`] /
/// [`BUFFER_STORAGE_HANDLE_OFFSET`]). Without an explicit repr the
/// layout (and even the field/tag offsets) would be unspecified. The
/// `Gc` payload is `repr(transparent)` over `u32`, so the union is
/// 4-byte and the handle sits immediately after the 4-byte tag.
/// Declaration order fixes the tag values: `Local == 0`, `Shared == 1`
/// ([`BUFFER_STORAGE_LOCAL_TAG`] / [`BUFFER_STORAGE_SHARED_TAG`]).
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
#[repr(C)]
pub enum BufferStorage {
    /// Non-shared backing — GC body holds the inline `Vec<u8>`.
    Local(LocalArrayBufferHandle),
    /// Cross-thread backing — GC body wraps `Arc<SharedBody>` with a
    /// real `Mutex<Vec<u8>>` and a process-unique id.
    Shared(SharedArrayBufferHandle),
}

/// Discriminant tag of [`BufferStorage::Local`] (declaration order 0).
pub const BUFFER_STORAGE_LOCAL_TAG: u32 = 0;
/// Discriminant tag of [`BufferStorage::Shared`] (declaration order 1).
pub const BUFFER_STORAGE_SHARED_TAG: u32 = 1;
/// Byte offset of the `#[repr(C)]` discriminant within [`BufferStorage`]
/// (0 — the tag leads the layout).
pub const BUFFER_STORAGE_DISCRIMINANT_OFFSET: usize = 0;
/// Byte offset of the compressed handle payload within [`BufferStorage`]
/// (4 — immediately after the 4-byte tag, both variants share it).
pub const BUFFER_STORAGE_HANDLE_OFFSET: usize = 4;

// The JIT inline element path depends on this exact layout. Lock it.
const _: () = {
    assert!(std::mem::size_of::<BufferStorage>() == 8);
    assert!(std::mem::align_of::<BufferStorage>() == 4);
    assert!(std::mem::size_of::<LocalArrayBufferHandle>() == 4);
};

/// Storage for a `SharedArrayBuffer`. Lives behind `Arc` so the
/// substrate can ship across host threads.
#[derive(Debug)]
pub struct SharedBody {
    /// Process-unique id. Stable for the buffer's lifetime; used
    /// as the registry key for [`crate::atomics_wait`].
    id: u64,
    /// Raw bytes guarded by a mutex so racing host threads can
    /// observe a consistent state. Atomics ops hold the lock for
    /// the duration of the compare-and-swap / load / store.
    bytes: Mutex<Vec<u8>>,
    /// `Some(n)` for a growable shared buffer (§25.2.5.4).
    max_byte_length: Option<usize>,
    /// GC-budget reservation for the shared backing store. Active
    /// constructors install a thread-safe token so the bytes remain
    /// accounted until the final `Arc<SharedBody>` drops.
    _external: Option<otter_gc::SharedExternalMemory>,
}

impl SharedBody {
    /// Process-unique id. The Atomics wait registry uses this
    /// together with the byte index as its key.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl JsArrayBuffer {
    fn wrap_local(handle: LocalArrayBufferHandle) -> Self {
        Self {
            storage: BufferStorage::Local(handle),
        }
    }

    fn wrap_shared(handle: SharedArrayBufferHandle) -> Self {
        Self {
            storage: BufferStorage::Shared(handle),
        }
    }

    /// Rewrap a pre-existing non-shared GC handle. Used by
    /// `Value::as_array_buffer` and call sites that recover the
    /// wrapper from a tagged `Value`.
    #[must_use]
    pub fn from_local_handle(handle: LocalArrayBufferHandle) -> Self {
        Self::wrap_local(handle)
    }

    /// Rewrap a pre-existing shared GC handle. Used by
    /// `Value::as_array_buffer` and call sites that recover the
    /// wrapper from a tagged `Value`.
    #[must_use]
    pub fn from_shared_handle(handle: SharedArrayBufferHandle) -> Self {
        Self::wrap_shared(handle)
    }

    /// Allocate a fresh fixed-length buffer of `len` zero bytes
    /// (ECMA-262 §25.1.2.1). No external-memory accounting; intended
    /// for fixtures and infallible host-only callers that already
    /// bound `len`.
    pub fn new(heap: &mut otter_gc::GcHeap, len: usize) -> Result<Self, otter_gc::OutOfMemory> {
        let bytes = vec![0u8; len];
        let handle = alloc_local_array_buffer(heap, bytes, None, None)?;
        Ok(Self::wrap_local(handle))
    }

    /// Fallible fixed-length allocation that accounts the backing
    /// store as external memory and visits caller-provided roots if
    /// booking triggers emergency GC.
    pub fn try_new_with_roots(
        len: usize,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Option<Self>, otter_gc::OutOfMemory> {
        let external = heap.reserve_external_with_roots(len as u64, external_visit)?;
        let mut bytes: Vec<u8> = Vec::new();
        if bytes.try_reserve_exact(len).is_err() {
            return Ok(None);
        }
        bytes.resize(len, 0u8);
        let handle = alloc_local_array_buffer(heap, bytes, None, Some(external))?;
        Ok(Some(Self::wrap_local(handle)))
    }

    /// Accounted resizable allocation. The reservation covers
    /// `max_byte_length` because the vector reserves that capacity
    /// up front and later `resize` calls only publish bytes already
    /// booked against the heap.
    pub fn new_resizable_with_roots(
        len: usize,
        max_byte_length: usize,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Option<Self>, otter_gc::OutOfMemory> {
        let external = heap.reserve_external_with_roots(max_byte_length as u64, external_visit)?;
        let mut bytes: Vec<u8> = Vec::new();
        if bytes.try_reserve_exact(max_byte_length).is_err() {
            return Ok(None);
        }
        bytes.resize(len, 0u8);
        let handle = alloc_local_array_buffer(heap, bytes, Some(max_byte_length), Some(external))?;
        Ok(Some(Self::wrap_local(handle)))
    }

    /// Wrap an existing byte vector and account its current length as
    /// external memory. Used by [`JsArrayBuffer::slice`] and
    /// `transfer` / `transferToFixedLength`.
    pub fn from_bytes_with_roots(
        bytes: Vec<u8>,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let external = heap.reserve_external_with_roots(bytes.len() as u64, external_visit)?;
        let handle = alloc_local_array_buffer(heap, bytes, None, Some(external))?;
        Ok(Self::wrap_local(handle))
    }

    /// Accounted fixed-length shared allocation. Release is tied to the
    /// `Arc<SharedBody>` lifetime so clones keep the backing store booked.
    pub fn try_new_shared_with_roots(
        len: usize,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Option<Self>, otter_gc::OutOfMemory> {
        let external = heap.reserve_shared_external_with_roots(len as u64, external_visit)?;
        let mut bytes: Vec<u8> = Vec::new();
        if bytes.try_reserve_exact(len).is_err() {
            return Ok(None);
        }
        bytes.resize(len, 0u8);
        let arc = Arc::new(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: None,
            _external: Some(external),
        });
        let handle = alloc_shared_array_buffer(heap, arc)?;
        Ok(Some(Self::wrap_shared(handle)))
    }

    /// Accounted growable shared allocation. The reservation covers
    /// `max_byte_length`, matching the capacity reserved up front.
    pub fn new_shared_growable_with_roots(
        len: usize,
        max_byte_length: usize,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Option<Self>, otter_gc::OutOfMemory> {
        let external =
            heap.reserve_shared_external_with_roots(max_byte_length as u64, external_visit)?;
        let mut bytes: Vec<u8> = Vec::new();
        if bytes.try_reserve_exact(max_byte_length).is_err() {
            return Ok(None);
        }
        bytes.resize(len, 0u8);
        let arc = Arc::new(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: Some(max_byte_length),
            _external: Some(external),
        });
        let handle = alloc_shared_array_buffer(heap, arc)?;
        Ok(Some(Self::wrap_shared(handle)))
    }

    /// Accounted external byte count for focused tests.
    #[cfg(test)]
    #[must_use]
    pub fn shared_external_bytes_for_test(self, heap: &otter_gc::GcHeap) -> Option<u64> {
        let BufferStorage::Shared(h) = self.storage else {
            return None;
        };
        heap.read_payload(h, |body| {
            body.inner
                ._external
                .as_ref()
                .map(otter_gc::SharedExternalMemory::bytes)
        })
    }

    /// `true` for a `SharedArrayBuffer`. Variant check; no heap
    /// access required.
    #[must_use]
    pub fn is_shared(self) -> bool {
        matches!(self.storage, BufferStorage::Shared(_))
    }

    /// `true` for a growable `SharedArrayBuffer` (the SAB
    /// equivalent of resizable). Reads the Shared body once.
    #[must_use]
    pub fn is_growable(self, heap: &otter_gc::GcHeap) -> bool {
        match self.storage {
            BufferStorage::Shared(h) => {
                heap.read_payload(h, |body| body.inner.max_byte_length.is_some())
            }
            BufferStorage::Local(_) => false,
        }
    }

    /// §25.2.5.4 — `SharedArrayBuffer.prototype.grow(newByteLength)`.
    /// Growing only; `new_len < current_len` returns `false`.
    pub fn grow(self, heap: &otter_gc::GcHeap, new_len: usize) -> bool {
        let BufferStorage::Shared(h) = self.storage else {
            return false;
        };
        // Clone Arc out of the GC body so the mutex lock is held
        // without crossing the heap borrow.
        let arc = heap.read_payload(h, |body| body.inner.clone());
        let max = match arc.max_byte_length {
            Some(m) => m,
            None => return false,
        };
        if new_len > max {
            return false;
        }
        let Ok(mut bytes) = arc.bytes.lock() else {
            return false;
        };
        if new_len < bytes.len() {
            return false;
        }
        bytes.resize(new_len, 0u8);
        true
    }

    /// Current byte length. `0` for a detached buffer.
    #[must_use]
    pub fn byte_length(self, heap: &otter_gc::GcHeap) -> usize {
        match self.storage {
            BufferStorage::Local(h) => {
                heap.read_payload(h, |body| if body.detached { 0 } else { body.bytes().len() })
            }
            BufferStorage::Shared(h) => {
                let arc = heap.read_payload(h, |body| body.inner.clone());
                arc.bytes.lock().map(|g| g.len()).unwrap_or(0)
            }
        }
    }

    /// Maximum byte length for a resizable / growable buffer;
    /// equals [`Self::byte_length`] for a fixed-length buffer
    /// per §25.1.4.6 `get ArrayBuffer.prototype.maxByteLength`.
    #[must_use]
    pub fn max_byte_length(self, heap: &otter_gc::GcHeap) -> usize {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| {
                if body.detached {
                    return 0;
                }
                body.max_byte_length.unwrap_or(body.bytes().len())
            }),
            BufferStorage::Shared(h) => {
                let arc = heap.read_payload(h, |body| body.inner.clone());
                arc.max_byte_length
                    .unwrap_or_else(|| arc.bytes.lock().map(|g| g.len()).unwrap_or(0))
            }
        }
    }

    /// `true` when the buffer was constructed with a `maxByteLength`
    /// argument (§25.1.4.7 `get ArrayBuffer.prototype.resizable`).
    #[must_use]
    pub fn is_resizable(self, heap: &otter_gc::GcHeap) -> bool {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| body.max_byte_length.is_some()),
            BufferStorage::Shared(_) => false,
        }
    }

    /// `true` once detach / transfer has happened (§25.1.3.1
    /// `IsDetachedBuffer`). `SharedArrayBuffer` is never detached
    /// per §25.2.4.1.
    #[must_use]
    pub fn is_detached(self, heap: &otter_gc::GcHeap) -> bool {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| body.detached),
            BufferStorage::Shared(_) => false,
        }
    }

    /// Closure-style read over the byte buffer. Callers must check
    /// [`Self::is_detached`] first; a detached buffer yields an
    /// empty slice. The closure runs under the heap borrow — never
    /// retain the slice past its scope.
    pub fn with_bytes<F, R>(self, heap: &otter_gc::GcHeap, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| {
                debug_assert!(
                    body.byte_cache_is_current(),
                    "a mutation path left the byte cache stale"
                );
                f(body.bytes())
            }),
            BufferStorage::Shared(h) => {
                let arc = heap.read_payload(h, |body| body.inner.clone());
                let guard = arc.bytes.lock().expect("SharedArrayBuffer mutex poisoned");
                f(&guard[..])
            }
        }
    }

    /// Closure-style mutable read over the byte buffer. Callers must
    /// check [`Self::is_detached`] first.
    pub fn with_bytes_mut<F, R>(self, heap: &mut otter_gc::GcHeap, f: F) -> R
    where
        F: FnOnce(&mut Vec<u8>) -> R,
    {
        match self.storage {
            BufferStorage::Local(h) => heap.with_payload(h, |body| body.with_bytes_mut(f)),
            BufferStorage::Shared(h) => {
                let arc = heap.read_payload(h, |body| body.inner.clone());
                let mut guard = arc.bytes.lock().expect("SharedArrayBuffer mutex poisoned");
                f(&mut guard)
            }
        }
    }

    /// Detach the buffer. Idempotent; subsequent calls are no-ops.
    /// `SharedArrayBuffer` rejects detach per §25.2.4.1 step 2 —
    /// the call is a no-op there.
    pub fn detach(self, heap: &mut otter_gc::GcHeap) {
        let BufferStorage::Local(h) = self.storage else {
            return;
        };
        heap.with_payload(h, |body| {
            if !body.detached {
                body.detached = true;
                body.with_bytes_mut(Vec::clear);
                let _ = body.external.take();
            }
        });
    }

    /// Resize a resizable buffer. Returns `false` when the buffer is
    /// fixed-length, detached, or `new_len` exceeds the recorded
    /// `maxByteLength`. Length growth zero-fills new bytes per
    /// §25.1.4.4 step 8.
    pub fn resize(self, heap: &mut otter_gc::GcHeap, new_len: usize) -> bool {
        let BufferStorage::Local(h) = self.storage else {
            return false;
        };
        heap.with_payload(h, |body| {
            if body.detached {
                return false;
            }
            let max = match body.max_byte_length {
                Some(m) => m,
                None => return false,
            };
            if new_len > max {
                return false;
            }
            body.with_bytes_mut(|bytes| bytes.resize(new_len, 0u8));
            true
        })
    }

    /// Subclass `[[Prototype]]` stamped at construction, if any. Shared
    /// buffers keep the realm default (subclassing stamps only locals).
    #[must_use]
    pub fn custom_proto(self, heap: &otter_gc::GcHeap) -> Option<crate::Value> {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| body.prototype_override),
            BufferStorage::Shared(h) => heap.read_payload(h, |body| body.prototype_override),
        }
    }

    /// Stamp the construction-time subclass prototype (local buffers).
    pub fn set_custom_proto(self, heap: &mut otter_gc::GcHeap, proto: crate::Value) {
        match self.storage {
            BufferStorage::Local(h) => {
                heap.with_payload(h, |body| body.prototype_override = Some(proto));
                heap.record_write(h, &proto);
            }
            BufferStorage::Shared(h) => {
                heap.with_payload(h, |body| body.prototype_override = Some(proto));
                heap.record_write(h, &proto);
            }
        }
    }

    /// Lazy expando bag for ordinary own properties. `None` until the
    /// first own-property write. Shared buffers keep a per-isolate
    /// expando on the GC wrapper (not the cross-thread `Arc`).
    #[must_use]
    pub fn expando(self, heap: &otter_gc::GcHeap) -> Option<crate::object::JsObject> {
        match self.storage {
            BufferStorage::Local(h) => heap.read_payload(h, |body| body.expando),
            BufferStorage::Shared(h) => heap.read_payload(h, |body| body.expando),
        }
    }

    /// Install the expando bag (first own-property write).
    pub fn set_expando(self, heap: &mut otter_gc::GcHeap, bag: crate::object::JsObject) {
        match self.storage {
            BufferStorage::Local(h) => {
                heap.with_payload(h, |body| body.expando = Some(bag));
                heap.record_write(h, &crate::Value::object(bag));
            }
            BufferStorage::Shared(h) => {
                heap.with_payload(h, |body| body.expando = Some(bag));
                heap.record_write(h, &crate::Value::object(bag));
            }
        }
    }

    /// Identity comparison. Compares the GC handle offset within the
    /// owning isolate; two wrappers of the same Local body / same
    /// Shared body compare equal because every per-isolate clone
    /// goes through the same handle.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.storage == other.storage
    }

    /// Backing-pointer for cycle / identity sets. Encodes the
    /// variant tag in the low bit so Local and Shared handles with
    /// the same offset stay distinct.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        let (tag, offset) = match self.storage {
            BufferStorage::Local(h) => (0u64, h.offset() as u64),
            BufferStorage::Shared(h) => (1u64, h.offset() as u64),
        };
        ((offset << 1) | tag) as usize as *const ()
    }

    /// Process-unique id for `SharedArrayBuffer`. `None` for a
    /// non-shared buffer. Used as the registry key for the
    /// `Atomics.wait` / `Atomics.notify` parking layer.
    #[must_use]
    pub fn shared_id(self, heap: &otter_gc::GcHeap) -> Option<u64> {
        match self.storage {
            BufferStorage::Local(_) => None,
            BufferStorage::Shared(h) => Some(heap.read_payload(h, |body| body.inner.id)),
        }
    }

    /// Clone the `Arc<SharedBody>` for cross-thread transfer.
    /// Returns `None` for a non-shared buffer. Slice 19c
    /// (`$262.agent.broadcast`) consumes this when shipping the
    /// SAB across the worker channel.
    #[must_use]
    pub fn as_shared_arc(self, heap: &otter_gc::GcHeap) -> Option<Arc<SharedBody>> {
        match self.storage {
            BufferStorage::Local(_) => None,
            BufferStorage::Shared(h) => Some(heap.read_payload(h, |body| body.inner.clone())),
        }
    }

    /// Rewrap an existing `Arc<SharedBody>` into a `JsArrayBuffer`.
    /// Used by the cross-thread message receiver path to reconstruct
    /// the JS-facing handle on the destination isolate without
    /// reallocating the byte storage.
    ///
    /// Allocates a fresh per-isolate GC handle that owns the
    /// incoming `Arc`. Subsequent reads / writes go through this
    /// handle; the underlying `SharedBody` (and its `Mutex`) stays
    /// shared across isolates.
    pub fn from_shared_arc(
        heap: &mut otter_gc::GcHeap,
        body: Arc<SharedBody>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let handle = alloc_shared_array_buffer(heap, body)?;
        Ok(Self::wrap_shared(handle))
    }

    /// Access the storage discriminator without exposing the inner
    /// handles. Useful for `match` over the family.
    #[must_use]
    pub fn storage(self) -> BufferStorage {
        self.storage
    }

    /// Visit the embedded GC handle slot during root tracing.
    pub fn trace_value_slots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        match self.storage {
            BufferStorage::Local(h) => {
                let p = &h as *const LocalArrayBufferHandle as *mut otter_gc::raw::RawGc;
                visitor(p);
            }
            BufferStorage::Shared(h) => {
                let p = &h as *const SharedArrayBufferHandle as *mut otter_gc::raw::RawGc;
                visitor(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cached `(data, byte_len)` pair is what compiled element access
    /// addresses, so every path that can move or resize the storage must leave
    /// it current. This walks each of those paths and asserts the invariant
    /// after it, which is what makes
    /// [`LOCAL_ARRAY_BUFFER_BODY_DATA_OFFSET`] a contract rather than a
    /// comment.
    fn assert_cache_current(buffer: JsArrayBuffer, heap: &otter_gc::GcHeap, what: &str) {
        let BufferStorage::Local(handle) = buffer.storage else {
            panic!("{what}: expected a local buffer");
        };
        let (current, data, len, vec_ptr, vec_len) = heap.read_payload(handle, |body| {
            (
                body.byte_cache_is_current(),
                body.data.get(),
                body.byte_len.get(),
                body.bytes().as_ptr().cast_mut(),
                body.bytes().len(),
            )
        });
        assert!(current, "{what}: cache stale (data={data:?} len={len})");
        assert_eq!(len, vec_len, "{what}: cached length");
        if vec_len != 0 {
            assert_eq!(data, vec_ptr, "{what}: cached base");
        }
    }

    #[test]
    fn byte_cache_stays_current_across_every_mutation_path() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");

        let empty = JsArrayBuffer::new(&mut heap, 0).expect("empty buffer");
        assert_cache_current(empty, &heap, "fresh empty");

        let buffer = JsArrayBuffer::new(&mut heap, 8).expect("buffer");
        assert_cache_current(buffer, &heap, "fresh");

        // A push past capacity reallocates, which is exactly the case a raw
        // `Vec`-first-word read would get wrong.
        buffer.with_bytes_mut(&mut heap, |bytes| {
            bytes.reserve_exact(0);
            for value in 0..4096u32 {
                bytes.push(value as u8);
            }
        });
        assert_cache_current(buffer, &heap, "after reallocating push");

        buffer.with_bytes_mut(&mut heap, |bytes| bytes.truncate(2));
        assert_cache_current(buffer, &heap, "after truncate");

        buffer.detach(&mut heap);
        assert_cache_current(buffer, &heap, "after detach");
        assert!(buffer.is_detached(&heap), "detach must stick");
    }

    #[test]
    fn resizable_byte_cache_tracks_growth_and_shrink() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let mut closure = |_: &mut dyn FnMut(*mut otter_gc::compressed::RawGc)| {};
        let visitor: &mut otter_gc::heap::RootSlotVisitor<'_> = &mut &mut closure;
        let buffer = JsArrayBuffer::new_resizable_with_roots(4, 64, &mut heap, visitor)
            .expect("reservation")
            .expect("resizable buffer");
        assert_cache_current(buffer, &heap, "fresh resizable");

        assert!(buffer.resize(&mut heap, 64), "grow to max");
        assert_cache_current(buffer, &heap, "after grow");

        assert!(buffer.resize(&mut heap, 1), "shrink");
        assert_cache_current(buffer, &heap, "after shrink");

        assert!(buffer.resize(&mut heap, 0), "shrink to empty");
        assert_cache_current(buffer, &heap, "after shrink to empty");
    }
}
