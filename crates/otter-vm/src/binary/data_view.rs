//! `DataView` value (ECMA-262 §25.3).
//!
//! A `DataView` is an object-shaped view over an
//! [`super::JsArrayBuffer`] that exposes typed access methods at
//! arbitrary byte offsets, with explicit byte-order control. Unlike
//! `TypedArray`, every `getX` / `setX` accepts an optional
//! `littleEndian` flag (default big-endian, matching §25.3.1.1).
//!
//! # Contents
//! - [`JsDataView`] — `Copy` GC handle to [`DataViewBodyGc`].
//! - [`DataViewBodyGc`] — GC body.
//!
//! # Invariants
//! - `byte_offset` and `byte_length` are construction-time values;
//!   the view is fixed-length. Bounds are validated against the
//!   backing buffer at construction time; subsequent buffer detach /
//!   resize is detected through `JsArrayBuffer` reader API on
//!   `buffer.is_detached(heap)` and friends.
//! - No `Rc` / `Arc` / `Cell` / `RefCell` inside the GC body.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-dataview-objects>

use otter_gc::raw::SlotVisitor;

use super::array_buffer::JsArrayBuffer;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`DataViewBodyGc`].
pub const DATA_VIEW_BODY_TYPE_TAG: u8 = 0x2a;

/// GC body for `Value::DataView` per ECMA-262 §25.3.
#[derive(Debug)]
pub struct DataViewBodyGc {
    /// Backing buffer (4-byte tagged handle to the backing buffer
    /// body — already `Copy`).
    pub buffer: JsArrayBuffer,
    /// Byte offset into the backing buffer (construction-time).
    pub byte_offset: usize,
    /// View byte length (construction-time).
    pub byte_length: usize,
}

impl otter_gc::SafeTraceable for DataViewBodyGc {
    const TYPE_TAG: u8 = DATA_VIEW_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, visitor: &mut SlotVisitor<'_>) {
        // The backing `JsArrayBuffer` owns its own GC handles
        // (`LocalArrayBufferHandle` / `SharedArrayBufferHandle`);
        // forward the trace so its body survives the cycle.
        self.buffer.trace_value_slots(visitor);
    }
}

/// 4-byte compressed GC handle to a [`DataViewBodyGc`]. `Copy`. Packs
/// into [`crate::Value`] under `TAG_PTR_OBJECT`.
pub type DataViewHandle = otter_gc::Gc<DataViewBodyGc>;

/// Allocate a DataView body on the GC heap.
///
/// # Errors
///
/// Surfaces [`otter_gc::OutOfMemory`] verbatim.
pub fn alloc_data_view(
    heap: &mut otter_gc::GcHeap,
    buffer: JsArrayBuffer,
    byte_offset: usize,
    byte_length: usize,
) -> Result<DataViewHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(DataViewBodyGc {
        buffer,
        byte_offset,
        byte_length,
    })
}

/// Cheap-to-copy `DataView` handle.
///
/// Backed by a 4-byte compressed GC handle; `Copy + Eq + Hash`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct JsDataView {
    handle: DataViewHandle,
}

impl JsDataView {
    /// Construct a fresh view. Caller must already have bounds-checked
    /// `byte_offset` and `byte_length` against the backing buffer
    /// (see §25.3.1.1 `DataView`).
    ///
    /// # Errors
    ///
    /// Surfaces [`otter_gc::OutOfMemory`] verbatim.
    pub fn new(
        heap: &mut otter_gc::GcHeap,
        buffer: JsArrayBuffer,
        byte_offset: usize,
        byte_length: usize,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let handle = alloc_data_view(heap, buffer, byte_offset, byte_length)?;
        Ok(Self { handle })
    }

    /// Rewrap an existing handle.
    #[must_use]
    pub fn from_handle(handle: DataViewHandle) -> Self {
        Self { handle }
    }

    /// Underlying GC handle.
    #[must_use]
    pub fn handle(self) -> DataViewHandle {
        self.handle
    }

    /// Backing buffer.
    #[must_use]
    pub fn buffer(self, heap: &otter_gc::GcHeap) -> JsArrayBuffer {
        heap.read_payload(self.handle, |body| body.buffer)
    }

    /// Byte offset into the backing buffer.
    #[must_use]
    pub fn byte_offset(self, heap: &otter_gc::GcHeap) -> usize {
        heap.read_payload(self.handle, |body| body.byte_offset)
    }

    /// View byte length.
    #[must_use]
    pub fn byte_length(self, heap: &otter_gc::GcHeap) -> usize {
        heap.read_payload(self.handle, |body| body.byte_length)
    }

    /// Identity comparison via GC handle offset.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.handle == other.handle
    }

    /// Backing-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        self.handle.offset() as usize as *const ()
    }

    /// Visit the embedded GC handle slot during root tracing.
    pub fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.handle as *const DataViewHandle as *mut otter_gc::raw::RawGc;
        visitor(p);
    }
}
