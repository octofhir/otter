//! JavaScript `ArrayBuffer` and `SharedArrayBuffer` (ECMA-262 §25.1
//! / §25.2).
//!
//! Storage split since slice 19b:
//!
//! - **ArrayBuffer (non-shared)** uses `Rc<LocalBody>` so the
//!   single-isolate fast path keeps `RefCell<Vec<u8>>` semantics
//!   for cheap shared mutation through the same handle.
//! - **SharedArrayBuffer** uses `Arc<SharedBody>` with a real
//!   `Mutex<Vec<u8>>`. The `Arc` survives cross-thread `Clone`,
//!   the `Mutex` synchronises racing reads/writes, and a
//!   process-unique `id: u64` keys the global Atomics wait
//!   registry in [`crate::atomics_wait`].
//!
//! # Contents
//! - [`JsArrayBuffer`] — cheap-to-clone handle.
//! - [`BufferStorage`] — `Local` / `Shared` discriminator.
//! - [`LocalBody`] / [`SharedBody`] — internal storage.
//! - [`BytesRef`] / [`BytesRefMut`] — unified borrow guard so
//!   prototype methods do not need to branch on storage.
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
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-sharedarraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-isdetachedbuffer>

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

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
#[derive(Debug)]
pub struct LocalArrayBufferBodyGc {
    /// Raw bytes. Empty when detached.
    pub bytes: Vec<u8>,
    /// `true` after detach / transfer; once set, stays set per spec.
    pub detached: bool,
    /// `Some(n)` for a resizable buffer; `None` for a fixed-length
    /// buffer.
    pub max_byte_length: Option<usize>,
    /// GC-budget reservation for the off-heap byte storage.
    pub external: Option<otter_gc::ExternalMemory>,
}

impl otter_gc::SafeTraceable for LocalArrayBufferBodyGc {
    const TYPE_TAG: u8 = LOCAL_ARRAY_BUFFER_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        // No outgoing GC slots — `Vec<u8>` is plain data.
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
    heap.alloc_old(LocalArrayBufferBodyGc {
        bytes,
        detached: false,
        max_byte_length,
        external,
    })
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
}

impl otter_gc::SafeTraceable for SharedArrayBufferBodyGc {
    const TYPE_TAG: u8 = SHARED_ARRAY_BUFFER_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        // Bytes live behind an `Arc` outside the cage.
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
    heap.alloc_old(SharedArrayBufferBodyGc { inner })
}

/// Cheap-to-clone `ArrayBuffer` / `SharedArrayBuffer` handle.
#[derive(Debug, Clone)]
pub struct JsArrayBuffer {
    storage: BufferStorage,
}

/// Storage discriminator. `Local` is the single-isolate
/// `ArrayBuffer` path; `Shared` is the cross-thread
/// `SharedArrayBuffer` path.
#[derive(Debug, Clone)]
pub enum BufferStorage {
    /// Non-shared backing — `Rc<LocalBody>` keeps the existing
    /// `RefCell<Vec<u8>>` semantics so single-isolate paths pay
    /// no synchronisation cost.
    Local(Rc<LocalBody>),
    /// Cross-thread backing — `Arc<SharedBody>` with a real
    /// `Mutex<Vec<u8>>` and a process-unique id.
    Shared(Arc<SharedBody>),
}

/// Storage for a non-shared `ArrayBuffer`.
#[derive(Debug)]
pub struct LocalBody {
    /// Raw bytes. Empty when detached.
    bytes: RefCell<Vec<u8>>,
    /// `true` after detach / transfer; once set, stays set per
    /// spec.
    detached: Cell<bool>,
    /// `Some(n)` for a resizable buffer; `None` for a
    /// fixed-length buffer. When set, [`Self::bytes`] never grows
    /// beyond `n`.
    max_byte_length: Option<usize>,
    /// GC-budget reservation for the off-heap byte storage. Older
    /// fixture-only constructors leave this empty; active runtime
    /// constructors install a token so backing stores participate in
    /// heap pressure.
    external: RefCell<Option<otter_gc::ExternalMemory>>,
}

/// Storage for a `SharedArrayBuffer`.
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

/// Read-only byte borrow that abstracts over the storage shape.
/// `Deref<Target = Vec<u8>>` so callers can keep their existing
/// `.len()` / indexing patterns regardless of variant.
pub enum BytesRef<'a> {
    /// `Ref<'_, Vec<u8>>` against a non-shared buffer.
    Local(Ref<'a, Vec<u8>>),
    /// Locked guard against a shared buffer.
    Shared(MutexGuard<'a, Vec<u8>>),
}

impl Deref for BytesRef<'_> {
    type Target = Vec<u8>;

    fn deref(&self) -> &Vec<u8> {
        match self {
            BytesRef::Local(r) => r,
            BytesRef::Shared(g) => g,
        }
    }
}

/// Mutable counterpart of [`BytesRef`]. `DerefMut<Target =
/// Vec<u8>>` so prototype methods that mutate the byte vector
/// (`fill` / `set` / `copy_from_slice`) keep working unchanged.
pub enum BytesRefMut<'a> {
    /// `RefMut<'_, Vec<u8>>` against a non-shared buffer.
    Local(RefMut<'a, Vec<u8>>),
    /// Locked guard against a shared buffer.
    Shared(MutexGuard<'a, Vec<u8>>),
}

impl Deref for BytesRefMut<'_> {
    type Target = Vec<u8>;

    fn deref(&self) -> &Vec<u8> {
        match self {
            BytesRefMut::Local(r) => r,
            BytesRefMut::Shared(g) => g,
        }
    }
}

impl DerefMut for BytesRefMut<'_> {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        match self {
            BytesRefMut::Local(r) => r,
            BytesRefMut::Shared(g) => g,
        }
    }
}

impl JsArrayBuffer {
    fn local(body: LocalBody) -> Self {
        Self {
            storage: BufferStorage::Local(Rc::new(body)),
        }
    }

    fn shared(body: SharedBody) -> Self {
        Self {
            storage: BufferStorage::Shared(Arc::new(body)),
        }
    }

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
        Self::try_new(len).unwrap_or_else(|| {
            Self::local(LocalBody {
                bytes: RefCell::new(Vec::new()),
                detached: Cell::new(true),
                max_byte_length: None,
                external: RefCell::new(None),
            })
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
        Some(Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: None,
            external: RefCell::new(None),
        }))
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
        Ok(Some(Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: None,
            external: RefCell::new(Some(external)),
        })))
    }

    /// Allocate a resizable buffer with initial length `len` and the
    /// given upper bound. Capacity is reserved up-front so subsequent
    /// `resize` calls never reallocate.
    #[must_use]
    pub fn new_resizable(len: usize, max_byte_length: usize) -> Self {
        let mut bytes = Vec::with_capacity(max_byte_length);
        bytes.resize(len, 0u8);
        Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: Some(max_byte_length),
            external: RefCell::new(None),
        })
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
        Ok(Some(Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: Some(max_byte_length),
            external: RefCell::new(Some(external)),
        })))
    }

    /// Wrap an existing byte vector. Used by [`JsArrayBuffer::slice`]
    /// and `transfer` / `transferToFixedLength`.
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: None,
            external: RefCell::new(None),
        })
    }

    /// Wrap an existing byte vector and account its current length as
    /// external memory.
    pub fn from_bytes_with_roots(
        bytes: Vec<u8>,
        heap: &mut otter_gc::GcHeap,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let external = heap.reserve_external_with_roots(bytes.len() as u64, external_visit)?;
        Ok(Self::local(LocalBody {
            bytes: RefCell::new(bytes),
            detached: Cell::new(false),
            max_byte_length: None,
            external: RefCell::new(Some(external)),
        }))
    }

    /// Allocate a fixed-length `SharedArrayBuffer`. Cannot be
    /// detached. The backing store is an `Arc<SharedBody>` with a
    /// real mutex, so the same buffer can be passed across host
    /// threads once the `$262.agent.*` worker harness lands in
    /// slice 19c.
    ///
    /// Returns a synthetic detached buffer when the allocation
    /// fails; [`Self::try_new_shared`] preserves the fallible
    /// shape for callers that need to surface a `RangeError`.
    #[must_use]
    pub fn new_shared(len: usize) -> Self {
        Self::try_new_shared(len).unwrap_or_else(|| {
            // Empty allocation; effectively a zero-length shared
            // buffer. SAB cannot transition into a detached state
            // per §25.2.4.1, so we expose a usable zero-length
            // buffer instead of a synthetic detached one.
            Self::shared(SharedBody {
                id: allocate_shared_id(),
                bytes: Mutex::new(Vec::new()),
                max_byte_length: None,
                _external: None,
            })
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
        Some(Self::shared(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: None,
            _external: None,
        }))
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
        Ok(Some(Self::shared(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: None,
            _external: Some(external),
        })))
    }

    /// Allocate a growable shared buffer per §25.2.5 — `length`
    /// floats up to `max_byte_length` via [`Self::grow`].
    #[must_use]
    pub fn new_shared_growable(len: usize, max_byte_length: usize) -> Self {
        let mut bytes = Vec::with_capacity(max_byte_length);
        bytes.resize(len, 0u8);
        Self::shared(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: Some(max_byte_length),
            _external: None,
        })
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
        Ok(Some(Self::shared(SharedBody {
            id: allocate_shared_id(),
            bytes: Mutex::new(bytes),
            max_byte_length: Some(max_byte_length),
            _external: Some(external),
        })))
    }

    /// Accounted external byte count for focused tests.
    #[cfg(test)]
    #[must_use]
    pub fn shared_external_bytes_for_test(&self) -> Option<u64> {
        let BufferStorage::Shared(body) = &self.storage else {
            return None;
        };
        body._external
            .as_ref()
            .map(otter_gc::SharedExternalMemory::bytes)
    }

    /// `true` for a `SharedArrayBuffer`.
    #[must_use]
    pub fn is_shared(&self) -> bool {
        matches!(self.storage, BufferStorage::Shared(_))
    }

    /// `true` for a growable `SharedArrayBuffer` (the SAB
    /// equivalent of resizable).
    #[must_use]
    pub fn is_growable(&self) -> bool {
        matches!(&self.storage, BufferStorage::Shared(s) if s.max_byte_length.is_some())
    }

    /// §25.2.5.4 — `SharedArrayBuffer.prototype.grow(newByteLength)`.
    /// Growing only; `new_len < current_len` returns `false`.
    pub fn grow(&self, new_len: usize) -> bool {
        let BufferStorage::Shared(body) = &self.storage else {
            return false;
        };
        let max = match body.max_byte_length {
            Some(m) => m,
            None => return false,
        };
        if new_len > max {
            return false;
        }
        let Ok(mut bytes) = body.bytes.lock() else {
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
    pub fn byte_length(&self) -> usize {
        match &self.storage {
            BufferStorage::Local(body) => {
                if body.detached.get() {
                    0
                } else {
                    body.bytes.borrow().len()
                }
            }
            BufferStorage::Shared(body) => body.bytes.lock().map(|g| g.len()).unwrap_or(0),
        }
    }

    /// Maximum byte length for a resizable / growable buffer;
    /// equals [`Self::byte_length`] for a fixed-length buffer
    /// per §25.1.4.6 `get ArrayBuffer.prototype.maxByteLength`.
    #[must_use]
    pub fn max_byte_length(&self) -> usize {
        match &self.storage {
            BufferStorage::Local(body) => {
                if body.detached.get() {
                    return 0;
                }
                body.max_byte_length
                    .unwrap_or_else(|| body.bytes.borrow().len())
            }
            BufferStorage::Shared(body) => body
                .max_byte_length
                .unwrap_or_else(|| body.bytes.lock().map(|g| g.len()).unwrap_or(0)),
        }
    }

    /// `true` when the buffer was constructed with a `maxByteLength`
    /// argument (§25.1.4.7 `get ArrayBuffer.prototype.resizable`).
    #[must_use]
    pub fn is_resizable(&self) -> bool {
        match &self.storage {
            BufferStorage::Local(body) => body.max_byte_length.is_some(),
            BufferStorage::Shared(_) => false,
        }
    }

    /// `true` once detach / transfer has happened (§25.1.3.1
    /// `IsDetachedBuffer`). `SharedArrayBuffer` is never detached
    /// per §25.2.4.1.
    #[must_use]
    pub fn is_detached(&self) -> bool {
        match &self.storage {
            BufferStorage::Local(body) => body.detached.get(),
            BufferStorage::Shared(_) => false,
        }
    }

    /// Borrow the bytes read-only. Callers must check
    /// [`Self::is_detached`] first.
    #[must_use]
    pub fn borrow_bytes(&self) -> BytesRef<'_> {
        match &self.storage {
            BufferStorage::Local(body) => BytesRef::Local(body.bytes.borrow()),
            BufferStorage::Shared(body) => {
                BytesRef::Shared(body.bytes.lock().expect("SharedArrayBuffer mutex poisoned"))
            }
        }
    }

    /// Borrow the bytes mutably. Callers must check
    /// [`Self::is_detached`] first.
    #[must_use]
    pub fn borrow_bytes_mut(&self) -> BytesRefMut<'_> {
        match &self.storage {
            BufferStorage::Local(body) => BytesRefMut::Local(body.bytes.borrow_mut()),
            BufferStorage::Shared(body) => {
                BytesRefMut::Shared(body.bytes.lock().expect("SharedArrayBuffer mutex poisoned"))
            }
        }
    }

    /// Detach the buffer. Idempotent; subsequent calls are no-ops.
    /// `SharedArrayBuffer` rejects detach per §25.2.4.1 step 2 —
    /// the call is a no-op there.
    pub fn detach(&self) {
        let BufferStorage::Local(body) = &self.storage else {
            return;
        };
        if !body.detached.replace(true) {
            body.bytes.borrow_mut().clear();
            let _ = body.external.borrow_mut().take();
        }
    }

    /// Resize a resizable buffer. Returns `false` when the buffer is
    /// fixed-length, detached, or `new_len` exceeds the recorded
    /// `maxByteLength`. Length growth zero-fills new bytes per
    /// §25.1.4.4 step 8.
    pub fn resize(&self, new_len: usize) -> bool {
        let BufferStorage::Local(body) = &self.storage else {
            return false;
        };
        if body.detached.get() {
            return false;
        }
        let max = match body.max_byte_length {
            Some(m) => m,
            None => return false,
        };
        if new_len > max {
            return false;
        }
        let mut bytes = body.bytes.borrow_mut();
        bytes.resize(new_len, 0u8);
        true
    }

    /// Identity comparison.
    #[must_use]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (&self.storage, &other.storage) {
            (BufferStorage::Local(a), BufferStorage::Local(b)) => Rc::ptr_eq(a, b),
            (BufferStorage::Shared(a), BufferStorage::Shared(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Backing-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(&self) -> *const () {
        match &self.storage {
            BufferStorage::Local(body) => Rc::as_ptr(body).cast(),
            BufferStorage::Shared(body) => Arc::as_ptr(body).cast(),
        }
    }

    /// Process-unique id for `SharedArrayBuffer`. `None` for a
    /// non-shared buffer. Used as the registry key for the
    /// `Atomics.wait` / `Atomics.notify` parking layer.
    #[must_use]
    pub fn shared_id(&self) -> Option<u64> {
        match &self.storage {
            BufferStorage::Local(_) => None,
            BufferStorage::Shared(body) => Some(body.id),
        }
    }

    /// Borrow the `Arc<SharedBody>` for cross-thread transfer.
    /// Returns `None` for a non-shared buffer. Slice 19c
    /// (`$262.agent.broadcast`) consumes this when shipping the
    /// SAB across the worker channel.
    #[must_use]
    pub fn as_shared_arc(&self) -> Option<&Arc<SharedBody>> {
        match &self.storage {
            BufferStorage::Local(_) => None,
            BufferStorage::Shared(body) => Some(body),
        }
    }

    /// Rewrap an existing `Arc<SharedBody>` into a `JsArrayBuffer`.
    /// Used by the cross-thread message receiver path to reconstruct
    /// the JS-facing handle on the destination isolate without
    /// reallocating the byte storage.
    #[must_use]
    pub fn from_shared_arc(body: Arc<SharedBody>) -> Self {
        Self {
            storage: BufferStorage::Shared(body),
        }
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
