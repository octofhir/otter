//! `DataView` value (ECMA-262 §25.3).
//!
//! A `DataView` is an object-shaped view over an
//! [`super::JsArrayBuffer`] that exposes typed access methods at
//! arbitrary byte offsets, with explicit byte-order control. Unlike
//! `TypedArray`, every `getX` / `setX` accepts an optional
//! `littleEndian` flag (default big-endian, matching §25.3.1.1).
//!
//! # Contents
//! - [`JsDataView`] — cheap-to-clone handle.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-dataview-objects>

use std::rc::Rc;

use otter_gc::raw::SlotVisitor;

use super::array_buffer::JsArrayBuffer;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`DataViewBodyGc`].
pub const DATA_VIEW_BODY_TYPE_TAG: u8 = 0x2a;

/// GC body for `Value::DataView` per ECMA-262 §25.3.
#[derive(Debug)]
pub struct DataViewBodyGc {
    /// Backing buffer.
    pub buffer: JsArrayBuffer,
    /// Byte offset into the backing buffer.
    pub byte_offset: usize,
    /// View byte length.
    pub byte_length: usize,
}

impl otter_gc::SafeTraceable for DataViewBodyGc {
    const TYPE_TAG: u8 = DATA_VIEW_BODY_TYPE_TAG;

    fn trace_slots_safe(&self, _visitor: &mut SlotVisitor<'_>) {
        // `JsArrayBuffer` storage lives outside the GC cage today;
        // the Rust drop releases the refcount when the body dies.
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

/// Cheap-to-clone DataView handle.
#[derive(Debug, Clone)]
pub struct JsDataView {
    inner: Rc<DataViewBody>,
}

/// Internal storage for a DataView.
#[derive(Debug)]
pub struct DataViewBody {
    /// Backing buffer.
    buffer: JsArrayBuffer,
    /// Byte offset into the buffer.
    byte_offset: usize,
    /// Byte length of the view.
    byte_length: usize,
}

impl JsDataView {
    /// Construct a fresh view. Caller must already have bounds-checked
    /// `byte_offset` and `byte_length` against the backing buffer
    /// (see §25.3.1.1 `DataView`).
    #[must_use]
    pub fn new(buffer: JsArrayBuffer, byte_offset: usize, byte_length: usize) -> Self {
        Self {
            inner: Rc::new(DataViewBody {
                buffer,
                byte_offset,
                byte_length,
            }),
        }
    }

    /// Backing buffer.
    #[must_use]
    pub fn buffer(&self) -> &JsArrayBuffer {
        &self.inner.buffer
    }

    /// Byte offset into the backing buffer.
    #[must_use]
    pub fn byte_offset(&self) -> usize {
        self.inner.byte_offset
    }

    /// View byte length.
    #[must_use]
    pub fn byte_length(&self) -> usize {
        self.inner.byte_length
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

impl PartialEq for JsDataView {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other)
    }
}
