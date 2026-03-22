//! JavaScript values with NaN-boxing
//!
//! NaN-boxing encodes JS values in 64 bits using the IEEE 754 NaN space.
//! This allows storing pointers, integers, and special values without
//! additional allocation.
//!
//! ## Encoding Scheme
//!
//! ```text
//! 64 bits: SEEEEEEE EEEEMMMM MMMMMMMM ... MMMMMMMM
//!          S = sign bit
//!          E = exponent (11 bits)
//!          M = mantissa (52 bits)
//!
//! Regular doubles: When exponent != 0x7FF (NaN)
//! NaN-boxed values: When exponent == 0x7FF and mantissa != 0 (quiet NaN)
//!
//! Encoding:
//! - Double:     stored directly (except NaN)
//! - NaN:        0x7FFA_0000_0000_0000 (canonical NaN, distinct from undefined)
//! - Integer:    0x7FF8_0001_XXXX_XXXX (32-bit signed in lower bits)
//! - Pointer:    0x7FFC_XXXX_XXXX_XXXX (48-bit pointer)
//! - Undefined:  0x7FF8_0000_0000_0000
//! - Null:       0x7FF8_0000_0000_0001
//! - True:       0x7FF8_0000_0000_0002
//! - False:      0x7FF8_0000_0000_0003
//! ```

use crate::array_buffer::JsArrayBuffer;
use crate::data_view::JsDataView;
use crate::gc::GcRef;
use crate::generator::JsGenerator;
use crate::map_data::{MapData, SetData};
use crate::object::{AccessorPair, JsObject};
use crate::promise::JsPromise;
use crate::proxy::JsProxy;
use crate::regexp::JsRegExp;
use crate::shared_buffer::SharedArrayBuffer;
use crate::string::JsString;
use crate::temporal_value::TemporalValue;
use crate::typed_array::JsTypedArray;
use std::cell::Cell;
use std::sync::Arc;

/// GC-managed interior of an upvalue cell.
///
/// Holds a single mutable `Value` via `Cell` (safe because the VM is
/// single-threaded within an Isolate).  The GC traces through this to
/// keep the captured value alive.
#[repr(C)]
pub struct UpvalueData {
    pub(crate) value: Cell<Value>,
}

// SAFETY: UpvalueData is only accessed from the single VM thread.
// Thread confinement is enforced by the Isolate abstraction.
// Cell<Value> is Send (Value is Send) but !Sync; the unsafe Sync impl
// is required so that GcRef<UpvalueData> satisfies Send+Sync bounds.
unsafe impl Sync for UpvalueData {}

impl UpvalueData {
    /// Create a new upvalue data cell.
    #[inline]
    pub fn new(value: Value) -> Self {
        Self {
            value: Cell::new(value),
        }
    }

    /// Read the current value (Copy, no locking).
    #[inline]
    pub fn get(&self) -> Value {
        self.value.get()
    }

    /// Write a new value (no locking).
    /// Includes generational write barrier for nursery GC.
    #[inline]
    pub fn set(&self, value: Value) {
        crate::object::gc_write_barrier(&value);
        self.value.set(value);
    }
}

impl otter_vm_gc::GcTraceable for UpvalueData {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::UPVALUE;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        self.value.get().trace(tracer);
    }
}

impl std::fmt::Debug for UpvalueData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UpvalueData({:?})", self.value.get())
    }
}

/// A reference to a GC-managed upvalue cell.
///
/// When a closure captures a local variable that may be mutated,
/// we store it in an UpvalueCell. Multiple closures can share
/// the same cell, enabling the counter pattern:
///
/// ```javascript
/// function counter() {
///     let count = 0;
///     return () => ++count;  // Increments shared count
/// }
/// ```
///
/// `UpvalueCell` is `Copy` (8 bytes — just a GcRef pointer).
/// The actual mutable value lives in the GC-managed `UpvalueData`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub struct UpvalueCell(pub(crate) GcRef<UpvalueData>);

impl UpvalueCell {
    /// Create a new upvalue cell with the given value (GC-allocated).
    pub fn new(value: Value) -> Self {
        Self(GcRef::new(UpvalueData::new(value)))
    }

    /// Get the current value from the cell.
    #[inline]
    pub fn get(&self) -> Value {
        self.0.get()
    }

    /// Set a new value in the cell.
    #[inline]
    pub fn set(&self, value: Value) {
        self.0.set(value);
    }

    /// Get the GC header (for tracing).
    #[inline]
    pub fn header(&self) -> &otter_vm_gc::GcHeader {
        self.0.header()
    }
}

// NaN-boxing constants
const QUIET_NAN: u64 = 0x7FF8_0000_0000_0000;
#[allow(dead_code)]
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// Tags (in the upper 16 bits after quiet NaN prefix)
const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;
const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;
const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;
const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;
const TAG_HOLE: u64 = 0x7FF8_0000_0000_0004; // Array hole sentinel (not user-visible)
const TAG_NAN: u64 = 0x7FFA_0000_0000_0000; // Canonical NaN (distinct from undefined)
const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;
const INT32_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
#[allow(dead_code)]
const TAG_POINTER: u64 = 0x7FFC_0000_0000_0000;

// Pointer sub-tags (Phase 1.1): encode the 3 hottest heap types in the NaN-box
// tag bits to avoid GcHeader dereference on type checks.
//
// All 4 values (0x7FFC..0x7FFF) are valid quiet NaN encodings.
// is_pointer:   (bits & 0xFFFC_0000_0000_0000) == 0x7FFC_0000_0000_0000
// ptr_subtag:   (bits >> 48) & 0x3 → 0=Object, 1=String, 2=Function, 3=Other
#[allow(dead_code)]
const TAG_PTR_OBJECT: u64 = 0x7FFC_0000_0000_0000; // JsObject (plain or array)
#[allow(dead_code)]
const TAG_PTR_STRING: u64 = 0x7FFD_0000_0000_0000; // JsString
#[allow(dead_code)]
const TAG_PTR_FUNCTION: u64 = 0x7FFE_0000_0000_0000; // Closure or NativeFunctionObject
#[allow(dead_code)]
const TAG_PTR_OTHER: u64 = 0x7FFF_0000_0000_0000; // Everything else (read GcHeader for subtype)

/// Mask to test for any pointer sub-tag (bits 50-63 must match 0x7FFC pattern).
#[allow(dead_code)]
const TAG_PTR_MASK: u64 = 0xFFFC_0000_0000_0000;

/// A JavaScript value using NaN-boxing for efficient 8-byte storage.
///
/// All type information is encoded in the upper 16 bits of the u64:
/// - Primitive tags: undefined, null, true, false, NaN, int32, hole
/// - Pointer sub-tags: TAG_PTR_OBJECT (0x7FFC), TAG_PTR_STRING (0x7FFD),
///   TAG_PTR_FUNCTION (0x7FFE), TAG_PTR_OTHER (0x7FFF)
/// - For TAG_PTR_OTHER, the GcHeader::tag() byte discriminates the exact type.
///
/// This type is `Copy` — GcRef pointers are stable heap addresses managed by GC.
#[repr(transparent)]
pub struct Value {
    bits: u64,
}

// SAFETY: Value is just a u64. Thread confinement is enforced by the Isolate.
unsafe impl Send for Value {}
unsafe impl Sync for Value {}

impl Copy for Value {}

impl Clone for Value {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

/// Native function handler type.
///
/// Receives `(this, args, &mut NativeContext)`. The `NativeContext` provides
/// access to the memory manager, global object, and — critically — the ability
/// to call JavaScript functions (closures or other natives) via
/// `ncx.call_function()`.
pub type NativeFn = Arc<
    dyn Fn(
            &Value,
            &[Value],
            &mut crate::context::NativeContext<'_>,
        ) -> std::result::Result<Value, crate::error::VmError>
        + Send
        + Sync,
>;

/// A JavaScript function closure
#[derive(Debug)]
pub struct Closure {
    /// Function index in the module
    pub function_index: u32,
    /// Reference to the module containing this function
    pub module: Arc<otter_vm_bytecode::Module>,
    /// Captured upvalues (heap-allocated cells for shared mutable access)
    pub upvalues: Vec<UpvalueCell>,
    /// Is this an async function
    pub is_async: bool,
    /// Is this a generator function
    pub is_generator: bool,
    /// Function object for properties like `.prototype` (GC-managed)
    pub object: GcRef<JsObject>,
    /// Home object for methods (ES2015 [[HomeObject]] internal slot)
    /// Used for `super` property access in class methods
    pub home_object: Option<GcRef<JsObject>>,
}

impl otter_vm_gc::GcTraceable for Closure {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::CLOSURE;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace function object
        tracer(self.object.header() as *const _);

        if let Some(home) = &self.home_object {
            tracer(home.header() as *const _);
        }

        // Trace each GC-managed upvalue cell
        for upvalue in &self.upvalues {
            tracer(upvalue.header() as *const _);
        }
    }
}

/// Opaque FFI call metadata for JIT fast path.
///
/// Created by `otter-ffi` when binding FFI symbols. The trampoline function
/// performs the actual C call (marshaling JS values to C types and back)
/// without requiring `otter-vm-core` to depend on `libffi`.
pub struct FfiCallInfo {
    /// Trampoline: `(opaque, fn_ptr, js_args_ptr, js_argc) -> NaN-boxed i64`
    ///
    /// - `opaque`: pointer to pre-built libffi state (CIF, arg types, return type)
    /// - `fn_ptr`: raw C function pointer from dlsym
    /// - `js_args_ptr`: pointer to array of NaN-boxed i64 JS argument values
    /// - `js_argc`: number of JS arguments
    pub trampoline: unsafe extern "C" fn(*const (), usize, *const i64, u16) -> i64,
    /// Raw C function pointer (from dlsym)
    pub fn_ptr: usize,
    /// Opaque data pointer (owned by creator, e.g. pre-built CIF + type info)
    pub opaque: *const (),
    /// Drop function for the opaque data
    pub opaque_drop: Option<unsafe fn(*const ())>,
    /// Expected argument count for quick validation
    pub arg_count: u16,
}

// SAFETY: FfiCallInfo is confined to a single-threaded VM isolate.
// The fn_ptr and opaque pointers are stable for the lifetime of the FFI library.
unsafe impl Send for FfiCallInfo {}
unsafe impl Sync for FfiCallInfo {}

impl Clone for FfiCallInfo {
    fn clone(&self) -> Self {
        // Shallow clone — opaque pointer is shared (the underlying CIF/type data is stable)
        Self {
            trampoline: self.trampoline,
            fn_ptr: self.fn_ptr,
            opaque: self.opaque,
            opaque_drop: None, // only the original owns the opaque data
            arg_count: self.arg_count,
        }
    }
}

impl Drop for FfiCallInfo {
    fn drop(&mut self) {
        if let Some(drop_fn) = self.opaque_drop
            && !self.opaque.is_null()
        {
            unsafe { drop_fn(self.opaque) };
        }
    }
}

/// A native function with an attached object for properties.
#[derive(Clone)]
pub struct NativeFunctionObject {
    /// The native function handler
    pub func: NativeFn,
    /// Attached object for properties (GC-managed)
    pub object: GcRef<JsObject>,
    /// Optional FFI call info for JIT fast path.
    /// When set, the JIT can bypass the NativeFn dispatch and call
    /// the C function directly through the trampoline.
    pub ffi_info: Option<Box<FfiCallInfo>>,
}

impl otter_vm_gc::GcTraceable for NativeFunctionObject {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::FUNCTION;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the attached object
        tracer(self.object.header() as *const _);

        // NativeFn is Arc<dyn Fn>, which is opaque to GC.
        // CONVENTION: NativeFn closures MUST NOT capture `GcRef<T>` or `Value`
        // containing heap references.  Values are received through the `this`/`args`
        // parameters on each call, not stored in the closure.  Violating this
        // convention can cause use-after-free if the GC collects the referenced
        // object while the closure is still alive.  See `promise.rs` for the only
        // known exception, which is documented with a SAFETY comment there.
    }
}

/// A JavaScript Symbol
#[derive(Debug)]
pub struct Symbol {
    /// Symbol description
    pub description: Option<String>,
    /// Unique ID
    pub id: u64,
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Symbol {}

impl std::hash::Hash for Symbol {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl otter_vm_gc::GcTraceable for Symbol {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::SYMBOL;
    fn trace(&self, _tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Symbol contains only primitives (String, u64), no GC references
    }
}

/// A JavaScript BigInt (arbitrary precision integer)
#[derive(Debug)]
pub struct BigInt {
    /// String representation (for now)
    pub value: String,
}

impl otter_vm_gc::GcTraceable for BigInt {
    const NEEDS_TRACE: bool = false;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::BIGINT;
    fn trace(&self, _tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // BigInt contains only a String, no GC references
    }
}

impl Value {
    /// Create undefined value
    #[inline]
    pub const fn undefined() -> Self {
        Self {
            bits: TAG_UNDEFINED,
        }
    }

    /// Create null value
    #[inline]
    pub const fn null() -> Self {
        Self { bits: TAG_NULL }
    }

    /// Create boolean value
    #[inline]
    pub const fn boolean(b: bool) -> Self {
        Self {
            bits: if b { TAG_TRUE } else { TAG_FALSE },
        }
    }

    /// Create an array hole sentinel.
    ///
    /// Holes represent absent elements in sparse arrays (e.g. `[1,,3]` or after
    /// `delete arr[i]`). They are never user-visible: `get()` converts them to
    /// `undefined`, and `has_own()` / `in` treats them as absent.
    #[inline]
    pub const fn hole() -> Self {
        Self { bits: TAG_HOLE }
    }

    /// Check if value is an array hole sentinel
    #[inline]
    pub fn is_hole(&self) -> bool {
        self.bits == TAG_HOLE
    }

    /// Create 32-bit integer value
    #[inline]
    pub fn int32(n: i32) -> Self {
        Self {
            bits: TAG_INT32 | (n as u32 as u64),
        }
    }

    /// Create number (f64) value
    #[inline]
    pub fn number(n: f64) -> Self {
        // Handle NaN specially to avoid collision with undefined
        if n.is_nan() {
            return Self { bits: TAG_NAN };
        }

        // Check if it fits in i32 for optimization, but preserve -0.0
        // Use 1.0/n to distinguish +0 (gives +inf) from -0 (gives -inf)
        if n.fract() == 0.0
            && n >= i32::MIN as f64
            && n <= i32::MAX as f64
            && (n != 0.0 || (1.0_f64 / n).is_sign_positive())
        {
            return Self::int32(n as i32);
        }

        Self { bits: n.to_bits() }
    }

    /// Construct a value from raw JIT bits when they are known to be non-pointer.
    ///
    /// Returns `None` for pointer-tagged values, since those require a valid
    /// `heap_ref` to be GC-safe.
    #[inline]
    pub(crate) fn from_jit_bits(bits: u64) -> Option<Self> {
        if (bits & TAG_PTR_MASK) == TAG_PTR_OBJECT {
            return None;
        }

        Some(Self { bits })
    }

    /// Reconstruct a full Value from raw NaN-boxed bits, including pointer types.
    ///
    /// For pointer-tagged values, reads the GC header tag to determine the type
    /// and validates the pointer. Returns `None` for unsupported
    /// or unrecognized pointer types.
    ///
    /// # Safety
    ///
    /// - The raw pointer in `bits` must point to a live GC object
    /// - No GC must occur while the returned Value is in use
    /// - Intended for JIT runtime helpers during no-GC JIT execution scope
    ///
    /// Reconstruct a Value from raw NaN-boxed bits.
    ///
    /// With `#[repr(transparent)]`, Value is just a u64, so this is trivial.
    /// Validates that pointer-tagged values have non-null payloads.
    #[allow(unsafe_code)]
    pub(crate) unsafe fn from_raw_bits_unchecked(bits: u64) -> Option<Self> {
        if (bits & TAG_PTR_MASK) == TAG_PTR_OBJECT {
            // Pointer-tagged value — validate non-null payload
            let raw_ptr = (bits & PAYLOAD_MASK) as *const u8;
            if raw_ptr.is_null() {
                return None;
            }
        }
        Some(Self { bits })
    }

    /// Return raw NaN-box bits for JIT argument passing.
    #[inline]
    #[allow(clippy::wrong_self_convention)]
    pub(crate) fn to_jit_bits(&self) -> i64 {
        self.bits as i64
    }

    /// Create NaN value explicitly
    #[inline]
    pub const fn nan() -> Self {
        Self { bits: TAG_NAN }
    }

    /// Create string value (GC-managed)
    pub fn string(s: GcRef<JsString>) -> Self {
        // Store pointer address in NaN-boxed format
        let ptr = s.as_ptr() as u64;
        Self {
            bits: TAG_PTR_STRING | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create object value (GC-managed)
    pub fn object(obj: GcRef<JsObject>) -> Self {
        let ptr = obj.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OBJECT | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create function closure value
    pub fn function(closure: GcRef<Closure>) -> Self {
        let ptr = closure.as_ptr() as u64;
        Self {
            bits: TAG_PTR_FUNCTION | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create promise value
    pub fn promise(promise: GcRef<JsPromise>) -> Self {
        let ptr = promise.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    pub fn regex(regex: GcRef<JsRegExp>) -> Self {
        let ptr = regex.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create proxy value
    pub fn proxy(proxy: GcRef<JsProxy>) -> Self {
        let ptr = proxy.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create generator value
    pub fn generator(generator: GcRef<JsGenerator>) -> Self {
        let ptr = generator.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create ArrayBuffer value
    pub fn array_buffer(ab: GcRef<JsArrayBuffer>) -> Self {
        let ptr = ab.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create TypedArray value
    pub fn typed_array(ta: GcRef<JsTypedArray>) -> Self {
        let ptr = ta.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create DataView value
    pub fn data_view(dv: GcRef<JsDataView>) -> Self {
        let ptr = dv.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create SharedArrayBuffer value
    pub fn shared_array_buffer(sab: GcRef<SharedArrayBuffer>) -> Self {
        let ptr = sab.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create Map internal data value
    pub fn map_data(data: GcRef<MapData>) -> Self {
        let ptr = data.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create Set internal data value
    pub fn set_data(data: GcRef<SetData>) -> Self {
        let ptr = data.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create ephemeron table value (for WeakMap/WeakSet)
    pub fn ephemeron_table(table: GcRef<otter_vm_gc::EphemeronTable>) -> Self {
        let ptr = table.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a WeakRef value (GC-managed weak reference)
    pub fn weak_ref(cell: GcRef<otter_vm_gc::WeakRefCell>) -> Self {
        let ptr = cell.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a FinalizationRegistry value (GC-managed)
    pub fn finalization_registry(data: GcRef<otter_vm_gc::FinalizationRegistryData>) -> Self {
        let ptr = data.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a Temporal value (PlainDate, PlainTime, Duration, etc.)
    pub fn temporal(tv: TemporalValue) -> Self {
        let gc = GcRef::new(tv);
        let ptr = gc.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a Temporal value from a pre-allocated GcRef
    pub fn temporal_ref(gc: GcRef<TemporalValue>) -> Self {
        let ptr = gc.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create array value (GC-managed)
    pub fn array(arr: GcRef<JsObject>) -> Self {
        let ptr = arr.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OBJECT | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create BigInt value
    pub fn bigint(value: String) -> Self {
        let bi = GcRef::new(BigInt { value });
        let ptr = bi.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create Symbol value
    pub fn symbol(sym: GcRef<Symbol>) -> Self {
        let ptr = sym.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create accessor pair value (for accessor property slots)
    pub fn accessor_pair(pair: GcRef<AccessorPair>) -> Self {
        let ptr = pair.as_ptr() as u64;
        Self {
            bits: TAG_PTR_OTHER | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create native function value
    pub fn native_function<F>(f: F, _memory_manager: Arc<crate::memory::MemoryManager>) -> Self
    where
        F: Fn(
                &Value,
                &[Value],
                &mut crate::context::NativeContext<'_>,
            ) -> Result<Value, crate::error::VmError>
            + Send
            + Sync
            + 'static,
    {
        let func: NativeFn = Arc::new(f);
        let object = GcRef::new(JsObject::new(Value::null()));
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        let ptr = native.as_ptr() as u64;
        Self {
            bits: TAG_PTR_FUNCTION | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a native function value from a pre-built `NativeFn` Arc.
    ///
    /// This avoids re-wrapping closures when the `NativeFn` is already available
    /// (e.g., from `#[dive]` macro-generated `_native_fn()` getters).
    pub fn native_function_from_arc(
        func: NativeFn,
        _memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        let object = GcRef::new(JsObject::new(Value::null()));
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        let ptr = native.as_ptr() as u64;
        Self {
            bits: TAG_PTR_FUNCTION | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a native function value from a `#[dive]` decl tuple `(name, fn, length)`.
    ///
    /// Sets `name` and `length` properties per ES2023 §10.2.8:
    /// `{ writable: false, enumerable: false, configurable: true }`.
    pub fn native_function_from_decl(
        name: &str,
        func: NativeFn,
        length: u32,
        _memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        let object = GcRef::new(JsObject::new(Value::null()));
        object.define_property(
            crate::object::PropertyKey::string("length"),
            crate::object::PropertyDescriptor::function_length(Value::int32(length as i32)),
        );
        object.define_property(
            crate::object::PropertyKey::string("name"),
            crate::object::PropertyDescriptor::function_length(Value::string(
                crate::string::JsString::intern(name),
            )),
        );
        // Built-in methods are not constructors (ES2023 §17)
        let _ = object.set(
            crate::object::PropertyKey::string("__non_constructor"),
            Value::boolean(true),
        );
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        let ptr = native.as_ptr() as u64;
        Self {
            bits: TAG_PTR_FUNCTION | (ptr & PAYLOAD_MASK),
        }
    }

    /// Create a native function value with a specific [[Prototype]].
    ///
    /// Per ES2023 §10.3.1, built-in function objects must have
    /// `%Function.prototype%` as their `[[Prototype]]`. Use this
    /// constructor when `Function.prototype` is already available.
    pub fn native_function_with_proto<F>(
        f: F,
        _memory_manager: Arc<crate::memory::MemoryManager>,
        prototype: GcRef<JsObject>,
    ) -> Self
    where
        F: Fn(
                &Value,
                &[Value],
                &mut crate::context::NativeContext<'_>,
            ) -> Result<Value, crate::error::VmError>
            + Send
            + Sync
            + 'static,
    {
        let func: NativeFn = Arc::new(f);
        let object = GcRef::new(JsObject::new(Value::object(prototype)));
        // Per ES2023 §10.2.8, built-in function objects have `length` and `name`
        // properties: { writable: false, enumerable: false, configurable: true }.
        object.define_property(
            crate::object::PropertyKey::string("length"),
            crate::object::PropertyDescriptor::function_length(Value::int32(0)),
        );
        object.define_property(
            crate::object::PropertyKey::string("name"),
            crate::object::PropertyDescriptor::function_length(Value::string(
                crate::string::JsString::intern(""),
            )),
        );
        // Built-in methods are not constructors (ES2023 §17)
        let _ = object.set(
            crate::object::PropertyKey::string("__non_constructor"),
            Value::boolean(true),
        );
        if let Some(realm_id) = prototype
            .get(&crate::object::PropertyKey::string("__realm_id__"))
            .and_then(|v| v.as_int32())
        {
            object.define_property(
                crate::object::PropertyKey::string("__realm_id__"),
                crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id)),
            );
        }
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        Self {
            bits: TAG_PTR_FUNCTION | (native.as_ptr() as u64 & PAYLOAD_MASK),
        }
    }

    /// Create a native function value with a specific [[Prototype]], name, and length.
    ///
    /// Like `native_function_with_proto` but sets correct `name`, `length`, and
    /// `__non_constructor` per ES2023 §10.2.8 / §17.
    pub fn native_function_with_proto_named<F>(
        f: F,
        _memory_manager: Arc<crate::memory::MemoryManager>,
        prototype: GcRef<JsObject>,
        name: &str,
        length: u32,
    ) -> Self
    where
        F: Fn(
                &Value,
                &[Value],
                &mut crate::context::NativeContext<'_>,
            ) -> Result<Value, crate::error::VmError>
            + Send
            + Sync
            + 'static,
    {
        let func: NativeFn = Arc::new(f);
        let object = GcRef::new(JsObject::new(Value::object(prototype)));
        object.define_property(
            crate::object::PropertyKey::string("length"),
            crate::object::PropertyDescriptor::function_length(Value::int32(length as i32)),
        );
        object.define_property(
            crate::object::PropertyKey::string("name"),
            crate::object::PropertyDescriptor::function_length(Value::string(
                crate::string::JsString::intern(name),
            )),
        );
        // Built-in methods are not constructors (ES2023 §17)
        let _ = object.set(
            crate::object::PropertyKey::string("__non_constructor"),
            Value::boolean(true),
        );
        if let Some(realm_id) = prototype
            .get(&crate::object::PropertyKey::string("__realm_id__"))
            .and_then(|v| v.as_int32())
        {
            object.define_property(
                crate::object::PropertyKey::string("__realm_id__"),
                crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id)),
            );
        }
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        Self {
            bits: TAG_PTR_FUNCTION | (native.as_ptr() as u64 & PAYLOAD_MASK),
        }
    }

    /// Create a native function value with a specific `[[Prototype]]` and
    /// a pre-existing object for properties (e.g., one that already has
    /// `length` and `name` set by `BuiltInBuilder`).
    pub fn native_function_with_proto_and_object(
        func: NativeFn,
        _memory_manager: Arc<crate::memory::MemoryManager>,
        prototype: GcRef<JsObject>,
        object: GcRef<JsObject>,
    ) -> Self {
        if let Some(realm_id) = prototype
            .get(&crate::object::PropertyKey::string("__realm_id__"))
            .and_then(|v| v.as_int32())
        {
            object.define_property(
                crate::object::PropertyKey::string("__realm_id__"),
                crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id)),
            );
        }
        let native = GcRef::new(NativeFunctionObject {
            func,
            object,
            ffi_info: None,
        });
        Self {
            bits: TAG_PTR_FUNCTION | (native.as_ptr() as u64 & PAYLOAD_MASK),
        }
    }

    /// Check if value is undefined
    #[inline]
    pub fn is_undefined(&self) -> bool {
        self.bits == TAG_UNDEFINED
    }

    /// Check if value is null
    #[inline]
    pub fn is_null(&self) -> bool {
        self.bits == TAG_NULL
    }

    /// Check if value is null or undefined
    #[inline]
    pub fn is_nullish(&self) -> bool {
        self.bits == TAG_UNDEFINED || self.bits == TAG_NULL
    }

    /// Check if value is a boolean
    #[inline]
    pub fn is_boolean(&self) -> bool {
        self.bits == TAG_TRUE || self.bits == TAG_FALSE
    }

    /// Check if value is an integer
    #[inline(always)]
    pub fn is_int32(&self) -> bool {
        (self.bits & INT32_TAG_MASK) == TAG_INT32
    }

    /// Check if value is NaN
    #[inline]
    pub fn is_nan(&self) -> bool {
        self.bits == TAG_NAN
    }

    /// Check if value is a number (including int32 and NaN)
    #[inline]
    pub fn is_number(&self) -> bool {
        self.is_int32() || self.is_nan() || !self.is_nan_boxed()
    }

    /// Check if value is a string
    #[inline(always)]
    #[allow(unsafe_code)]
    pub fn is_string(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_STRING
    }

    /// Check if value has [[IsHTMLDDA]] internal slot (Annex B)
    #[inline]
    pub fn is_htmldda(&self) -> bool {
        self.as_object().is_some_and(|o| o.is_htmldda())
    }

    /// Check if value is an object (includes functions, arrays, regexps, etc.)
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_object(&self) -> bool {
        use otter_vm_gc::object::tags as gc_tags;
        let tag16 = self.bits & TAG_MASK;
        match tag16 {
            TAG_PTR_OBJECT | TAG_PTR_FUNCTION => true,
            TAG_PTR_OTHER => {
                let t = unsafe { self.gc_header_tag_from_bits() };
                matches!(
                    t,
                    gc_tags::REGEXP
                        | gc_tags::PROMISE
                        | gc_tags::PROXY
                        | gc_tags::GENERATOR
                        | gc_tags::ARRAY_BUFFER
                        | gc_tags::TYPED_ARRAY
                        | gc_tags::DATA_VIEW
                )
            }
            _ => false,
        }
    }

    /// Check if value is a function (includes native functions)
    #[inline(always)]
    pub fn is_function(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_FUNCTION
    }

    /// Get unique function ID (payload of internal pointer)
    #[inline]
    pub fn function_id(&self) -> u64 {
        if self.is_function() {
            self.bits & PAYLOAD_MASK
        } else {
            0
        }
    }

    /// Check if value is a promise
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_promise(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::PROMISE
    }

    /// Check if value is a proxy
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_proxy(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::PROXY
    }

    /// Check if value is a generator
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_generator(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::GENERATOR
    }

    /// Check if value is an ArrayBuffer
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_array_buffer(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::ARRAY_BUFFER
    }

    /// Check if value is a TypedArray
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_typed_array(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::TYPED_ARRAY
    }

    /// Check if value is a DataView
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_data_view(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::DATA_VIEW
    }

    /// Check if value is a SharedArrayBuffer
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_shared_array_buffer(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() }
                == otter_vm_gc::object::tags::SHARED_ARRAY_BUFFER
    }

    /// Check if value is a native function
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_native_function(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_FUNCTION
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::FUNCTION
    }

    /// Check if value is callable (function, native function, or bound function)
    #[inline]
    pub fn is_callable(&self) -> bool {
        if self.is_function() {
            return true;
        }
        if let Some(proxy) = self.as_proxy() {
            if let Some(target) = proxy.target() {
                return target.is_callable();
            }
            return proxy.target_raw().is_callable();
        }
        // Bound functions are plain objects with __boundFunction__ property
        if let Some(obj) = self.as_object()
            && obj
                .get(&crate::object::PropertyKey::string("__boundFunction__"))
                .is_some()
        {
            return true;
        }
        false
    }

    /// Check if value is a symbol
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_symbol(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::SYMBOL
    }

    /// Check if value is a BigInt
    #[inline]
    #[allow(unsafe_code)]
    pub fn is_bigint(&self) -> bool {
        (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::BIGINT
    }

    /// Check if this is a NaN-boxed value (vs regular double)
    #[inline]
    fn is_nan_boxed(&self) -> bool {
        // Quiet NaN pattern: exponent all 1s, quiet bit set
        (self.bits & QUIET_NAN) == QUIET_NAN
    }

    /// Get as boolean
    pub fn as_boolean(&self) -> Option<bool> {
        match self.bits {
            TAG_TRUE => Some(true),
            TAG_FALSE => Some(false),
            _ => None,
        }
    }

    /// Get as 32-bit integer
    pub fn as_int32(&self) -> Option<i32> {
        if self.is_int32() {
            Some((self.bits & 0xFFFF_FFFF) as i32)
        } else {
            None
        }
    }

    /// Get as number (f64)
    pub fn as_number(&self) -> Option<f64> {
        if self.is_int32() {
            Some((self.bits & 0xFFFF_FFFF) as i32 as f64)
        } else if self.bits == TAG_NAN {
            Some(f64::NAN)
        } else if !self.is_nan_boxed() {
            Some(f64::from_bits(self.bits))
        } else {
            None
        }
    }

    /// Get as string (GC-managed)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_string(&self) -> Option<GcRef<JsString>> {
        if (self.bits & TAG_MASK) == TAG_PTR_STRING {
            Some(unsafe { self.extract_gcref::<JsString>() })
        } else {
            None
        }
    }

    /// Get as object (GC-managed)
    ///
    /// Returns the inner `JsObject` for objects, arrays, functions, generators,
    /// regexps, array buffers, and typed arrays.
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_object(&self) -> Option<GcRef<JsObject>> {
        let tag16 = self.bits & TAG_MASK;
        match tag16 {
            TAG_PTR_OBJECT => {
                // Plain objects and arrays both store JsObject directly
                Some(unsafe { self.extract_gcref::<JsObject>() })
            }
            TAG_PTR_FUNCTION => {
                // Closure or NativeFunction — extract inner .object field
                let gc_tag = unsafe { self.gc_header_tag_from_bits() };
                if gc_tag == otter_vm_gc::object::tags::CLOSURE {
                    let closure: GcRef<Closure> = unsafe { self.extract_gcref() };
                    Some(closure.object)
                } else {
                    // FUNCTION tag = NativeFunctionObject
                    let nfo: GcRef<NativeFunctionObject> = unsafe { self.extract_gcref() };
                    Some(nfo.object)
                }
            }
            TAG_PTR_OTHER => {
                let gc_tag = unsafe { self.gc_header_tag_from_bits() };
                match gc_tag {
                    otter_vm_gc::object::tags::GENERATOR => {
                        let g: GcRef<JsGenerator> = unsafe { self.extract_gcref() };
                        Some(g.object)
                    }
                    otter_vm_gc::object::tags::REGEXP => {
                        let r: GcRef<JsRegExp> = unsafe { self.extract_gcref() };
                        Some(r.object)
                    }
                    otter_vm_gc::object::tags::ARRAY_BUFFER => {
                        let ab: GcRef<JsArrayBuffer> = unsafe { self.extract_gcref() };
                        Some(ab.object)
                    }
                    otter_vm_gc::object::tags::TYPED_ARRAY => {
                        let ta: GcRef<JsTypedArray> = unsafe { self.extract_gcref() };
                        Some(ta.object)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Get as array object (GC-managed)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_array(&self) -> Option<GcRef<JsObject>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OBJECT
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::ARRAY
        {
            Some(unsafe { self.extract_gcref::<JsObject>() })
        } else {
            None
        }
    }

    /// Get as function closure
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_function(&self) -> Option<GcRef<Closure>> {
        if (self.bits & TAG_MASK) == TAG_PTR_FUNCTION
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::CLOSURE
        {
            Some(unsafe { self.extract_gcref::<Closure>() })
        } else {
            None
        }
    }

    /// Get the inner `JsObject` attached to a function (closure or native).
    /// This is the object that holds properties like `.prototype`, `.name`, `.length`
    /// and carries the `[[Prototype]]` internal slot (should be `Function.prototype`).
    #[inline]
    #[allow(unsafe_code)]
    pub fn function_inner_object(&self) -> Option<GcRef<JsObject>> {
        if (self.bits & TAG_MASK) != TAG_PTR_FUNCTION {
            return None;
        }
        let gc_tag = unsafe { self.gc_header_tag_from_bits() };
        if gc_tag == otter_vm_gc::object::tags::CLOSURE {
            let closure: GcRef<Closure> = unsafe { self.extract_gcref() };
            Some(closure.object)
        } else if gc_tag == otter_vm_gc::object::tags::FUNCTION {
            let nfo: GcRef<NativeFunctionObject> = unsafe { self.extract_gcref() };
            Some(nfo.object)
        } else {
            None
        }
    }

    /// Get as native function
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_native_function(&self) -> Option<&NativeFn> {
        if self.is_native_function() {
            let nfo: GcRef<NativeFunctionObject> = unsafe { self.extract_gcref() };
            // SAFETY: GcRef wraps a stable GC-heap pointer. The NativeFunctionObject
            // is alive as long as this Value's pointer is valid (i.e., it's rooted).
            // The returned reference lifetime is bounded by the caller's usage of &self.
            Some(unsafe { &(*nfo.as_ptr()).func })
        } else {
            None
        }
    }

    /// Get the properties object of a native function, for setting `name`, `length`, etc.
    /// Returns `None` if the value is not a `NativeFunction`.
    #[inline]
    #[allow(unsafe_code)]
    pub fn native_function_object(&self) -> Option<GcRef<JsObject>> {
        if self.is_native_function() {
            let nfo: GcRef<NativeFunctionObject> = unsafe { self.extract_gcref() };
            Some(nfo.object)
        } else {
            None
        }
    }

    /// Get as promise
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_promise(&self) -> Option<GcRef<JsPromise>> {
        if self.is_promise() {
            Some(unsafe { self.extract_gcref::<JsPromise>() })
        } else {
            None
        }
    }

    /// Get as proxy
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_proxy(&self) -> Option<GcRef<JsProxy>> {
        if self.is_proxy() {
            Some(unsafe { self.extract_gcref::<JsProxy>() })
        } else {
            None
        }
    }

    /// Get as generator
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_generator(&self) -> Option<GcRef<JsGenerator>> {
        if self.is_generator() {
            Some(unsafe { self.extract_gcref::<JsGenerator>() })
        } else {
            None
        }
    }

    /// Get as regex
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_regex(&self) -> Option<GcRef<JsRegExp>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::REGEXP
        {
            Some(unsafe { self.extract_gcref::<JsRegExp>() })
        } else {
            None
        }
    }

    /// Get as ArrayBuffer
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_array_buffer(&self) -> Option<GcRef<JsArrayBuffer>> {
        if self.is_array_buffer() {
            Some(unsafe { self.extract_gcref::<JsArrayBuffer>() })
        } else {
            None
        }
    }

    /// Get as TypedArray
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_typed_array(&self) -> Option<GcRef<JsTypedArray>> {
        if self.is_typed_array() {
            Some(unsafe { self.extract_gcref::<JsTypedArray>() })
        } else {
            None
        }
    }

    /// Get as DataView
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_data_view(&self) -> Option<GcRef<JsDataView>> {
        if self.is_data_view() {
            Some(unsafe { self.extract_gcref::<JsDataView>() })
        } else {
            None
        }
    }

    /// Get as SharedArrayBuffer
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_shared_array_buffer(&self) -> Option<GcRef<SharedArrayBuffer>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() }
                == otter_vm_gc::object::tags::SHARED_ARRAY_BUFFER
        {
            Some(unsafe { self.extract_gcref::<SharedArrayBuffer>() })
        } else {
            None
        }
    }

    /// Get as symbol
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_symbol(&self) -> Option<GcRef<Symbol>> {
        if self.is_symbol() {
            Some(unsafe { self.extract_gcref::<Symbol>() })
        } else {
            None
        }
    }

    /// Get as Map internal data
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_map_data(&self) -> Option<GcRef<MapData>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::MAP_DATA
        {
            Some(unsafe { self.extract_gcref::<MapData>() })
        } else {
            None
        }
    }

    /// Get as Set internal data
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_set_data(&self) -> Option<GcRef<SetData>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::SET_DATA
        {
            Some(unsafe { self.extract_gcref::<SetData>() })
        } else {
            None
        }
    }

    /// Get as ephemeron table
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_ephemeron_table(&self) -> Option<GcRef<otter_vm_gc::EphemeronTable>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() }
                == otter_vm_gc::object::tags::EPHEMERON_TABLE
        {
            Some(unsafe { self.extract_gcref::<otter_vm_gc::EphemeronTable>() })
        } else {
            None
        }
    }

    /// Get as WeakRef cell reference
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_weak_ref(&self) -> Option<GcRef<otter_vm_gc::WeakRefCell>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::WEAK_REF
        {
            Some(unsafe { self.extract_gcref::<otter_vm_gc::WeakRefCell>() })
        } else {
            None
        }
    }

    /// Get as FinalizationRegistry data reference
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_finalization_registry(&self) -> Option<GcRef<otter_vm_gc::FinalizationRegistryData>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() }
                == otter_vm_gc::object::tags::FINALIZATION_REGISTRY
        {
            Some(unsafe { self.extract_gcref::<otter_vm_gc::FinalizationRegistryData>() })
        } else {
            None
        }
    }

    /// Get as Temporal value (PlainDate, PlainTime, Duration, etc.)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_temporal(&self) -> Option<GcRef<TemporalValue>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::TEMPORAL
        {
            Some(unsafe { self.extract_gcref::<TemporalValue>() })
        } else {
            None
        }
    }

    // ========================================================================
    // Phase 1.1 helpers: pointer sub-tag + GcHeader-based type discrimination
    // ========================================================================

    /// Check if this value is a NaN-boxed pointer (any heap type).
    #[inline(always)]
    #[allow(dead_code)]
    fn is_pointer_tagged(&self) -> bool {
        (self.bits & TAG_PTR_MASK) == TAG_PTR_OBJECT
    }

    /// Get the raw 48-bit pointer from a pointer-tagged value.
    ///
    /// Returns the pointer to the T value inside `GcBox<T>` (not the GcBox itself).
    /// Returns null for non-pointer values.
    #[inline(always)]
    fn raw_heap_ptr(&self) -> *const u8 {
        (self.bits & PAYLOAD_MASK) as *const u8
    }

    /// Read the `GcHeader::tag()` byte from a pointer-tagged value.
    ///
    /// The NaN-boxed pointer points to the `value` field of `GcAllocation<T>`.
    /// The GcHeader is at the start of the allocation. All GC allocations are
    /// 16-byte aligned, so the header is at `(value_ptr - 8) & !15`:
    /// - For T with align ≤ 8: value at alloc+8, formula gives alloc ✓
    /// - For T with align 16: value at alloc+16, formula gives alloc ✓
    ///
    /// # Safety
    /// Caller must ensure this value is pointer-tagged and points to a live GcBox.
    #[inline(always)]
    #[allow(unsafe_code)]
    unsafe fn gc_header_tag_from_bits(&self) -> u8 {
        let raw_ptr = self.raw_heap_ptr() as usize;
        debug_assert!(raw_ptr != 0);
        // GcHeader is 8 bytes; allocation is 16-byte aligned.
        // value_ptr - 8 is inside the GcAllocation; rounding down to 16 gives the header.
        let header_ptr = ((raw_ptr - 8) & !15) as *const otter_vm_gc::GcHeader;
        unsafe { (*header_ptr).tag() }
    }

    /// Extract a `GcRef<T>` from NaN-boxed pointer bits.
    ///
    /// # Safety
    /// Caller must ensure this Value is a pointer of the correct type T.
    #[inline(always)]
    #[allow(unsafe_code)]
    unsafe fn extract_gcref<T>(&self) -> GcRef<T> {
        let raw_ptr = self.raw_heap_ptr();
        debug_assert!(!raw_ptr.is_null());
        let offset = std::mem::offset_of!(crate::gc::GcBox<T>, value);
        let box_ptr = unsafe { raw_ptr.sub(offset) as *mut crate::gc::GcBox<T> };
        unsafe {
            GcRef::from_gc(crate::gc::Gc::from_raw(std::ptr::NonNull::new_unchecked(
                box_ptr,
            )))
        }
    }

    /// Get as AccessorPair (for accessor property slots)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_accessor_pair(&self) -> Option<GcRef<AccessorPair>> {
        if (self.bits & TAG_MASK) == TAG_PTR_OTHER
            && unsafe { self.gc_header_tag_from_bits() } == otter_vm_gc::object::tags::ACCESSOR_PAIR
        {
            Some(unsafe { self.extract_gcref::<AccessorPair>() })
        } else {
            None
        }
    }

    // ========================================================================
    // Missing typed accessors (replacing direct heap_ref() matches)
    // ========================================================================

    /// Get as BigInt (GC-managed)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_bigint(&self) -> Option<GcRef<BigInt>> {
        if self.is_bigint() {
            Some(unsafe { self.extract_gcref::<BigInt>() })
        } else {
            None
        }
    }

    /// Get as native function object (GC-managed NativeFunctionObject)
    #[inline]
    #[allow(unsafe_code)]
    pub fn as_native_fn_obj(&self) -> Option<GcRef<NativeFunctionObject>> {
        if self.is_native_function() {
            Some(unsafe { self.extract_gcref::<NativeFunctionObject>() })
        } else {
            None
        }
    }

    /// Get the FFI call info pointer from a native function, if it has one.
    /// Returns a raw pointer that's valid as long as the NativeFunctionObject is alive.
    #[inline]
    #[allow(unsafe_code)]
    pub fn ffi_call_info(&self) -> Option<*const FfiCallInfo> {
        if self.is_native_function() {
            let nfo: GcRef<NativeFunctionObject> = unsafe { self.extract_gcref() };
            let nfo_ref = unsafe { &*nfo.as_ptr() };
            nfo_ref
                .ffi_info
                .as_ref()
                .map(|b| &**b as *const FfiCallInfo)
        } else {
            None
        }
    }

    /// Set FFI call info on a native function for JIT fast path.
    ///
    /// # Safety
    /// Must only be called on a Value that is a native function.
    #[inline]
    #[allow(unsafe_code)]
    pub unsafe fn set_ffi_call_info(&self, info: FfiCallInfo) {
        unsafe {
            if self.is_native_function() {
                let nfo: GcRef<NativeFunctionObject> = self.extract_gcref();
                let nfo_mut = &mut *(nfo.as_ptr() as *mut NativeFunctionObject);
                nfo_mut.ffi_info = Some(Box::new(info));
            }
        }
    }

    /// Get as closure (GC-managed Closure).
    /// Note: `as_function()` returns the same thing; this is an alias for clarity.
    pub fn as_closure(&self) -> Option<GcRef<Closure>> {
        self.as_function()
    }

    /// Get as map internal data (GC-managed)
    pub fn as_map_data_raw(&self) -> Option<GcRef<MapData>> {
        self.as_map_data()
    }

    /// Get as set internal data (GC-managed)
    pub fn as_set_data_raw(&self) -> Option<GcRef<SetData>> {
        self.as_set_data()
    }

    /// Get the NaN-boxed bits (for identity-based hashing/comparison).
    /// For pointer values, the lower 48 bits are the address — unique per object.
    #[inline(always)]
    pub fn to_bits_raw(&self) -> u64 {
        self.bits
    }

    /// Reconstruct a Value from raw NaN-boxed bits.
    ///
    /// # Safety
    /// The caller must ensure `bits` is a valid NaN-boxed value encoding.
    #[inline(always)]
    pub unsafe fn from_bits_raw(bits: u64) -> Self {
        Self { bits }
    }

    /// Get the GC header pointer for this value (if it's a GC-managed type)
    ///
    /// Returns the pointer to the GcHeader if this value contains a GC-managed
    /// object (String or Object), otherwise returns None.
    #[allow(unsafe_code)]
    pub fn gc_header(&self) -> Option<*const otter_vm_gc::GcHeader> {
        let tag16 = self.bits & TAG_MASK;
        match tag16 {
            TAG_PTR_OBJECT | TAG_PTR_STRING | TAG_PTR_FUNCTION | TAG_PTR_OTHER => {
                let raw_ptr = self.raw_heap_ptr() as usize;
                if raw_ptr == 0 {
                    return None;
                }
                // Same formula as gc_header_tag_from_bits: all GC allocations
                // are 16-byte aligned, so (value_ptr - 8) rounded down to 16
                // gives the header address.
                Some(((raw_ptr - 8) & !15) as *const otter_vm_gc::GcHeader)
            }
            _ => None,
        }
    }

    /// Convert to boolean (ToBoolean)
    pub fn to_boolean(&self) -> bool {
        match self.bits {
            TAG_UNDEFINED | TAG_NULL | TAG_FALSE | TAG_NAN | TAG_HOLE => false, // NaN is falsy, holes treated as undefined
            TAG_TRUE => true,
            _ if self.is_int32() => self.as_int32().unwrap() != 0,
            _ if !self.is_nan_boxed() => {
                let n = f64::from_bits(self.bits);
                !n.is_nan() && n != 0.0
            }
            _ => {
                if let Some(b) = self.as_bigint() {
                    return b.value != "0";
                }
                // Strings: empty string is false
                if let Some(s) = self.as_string() {
                    !s.is_empty()
                } else if self.is_htmldda() {
                    // [[IsHTMLDDA]] objects/native functions: ToBoolean returns false (Annex B)
                    false
                } else {
                    // Objects, functions are always truthy
                    true
                }
            }
        }
    }

    /// Get the type name (for typeof)
    pub fn type_of(&self) -> &'static str {
        use crate::object::PropertyKey;

        match self.bits {
            TAG_UNDEFINED | TAG_HOLE => "undefined",
            TAG_NULL => "object", // typeof null === "object" (historical bug)
            TAG_TRUE | TAG_FALSE => "boolean",
            TAG_NAN => "number", // NaN is a number
            _ if self.is_int32() || !self.is_nan_boxed() => "number",
            _ => {
                if self.is_string() {
                    "string"
                } else if self.is_symbol() {
                    "symbol"
                } else if self.is_bigint() {
                    "bigint"
                } else if self.is_function() {
                    if self.is_htmldda() {
                        "undefined"
                    } else {
                        "function"
                    }
                } else if self.is_proxy() {
                    if self.is_callable() {
                        "function"
                    } else {
                        "object"
                    }
                } else if self.is_object() {
                    // as_object covers plain objects, arrays, generators, regexp, etc.
                    // Check [[IsHTMLDDA]] and bound functions on plain objects
                    if let Some(obj) = self.as_object() {
                        if obj.is_htmldda() {
                            "undefined"
                        } else if obj.get(&PropertyKey::string("__boundFunction__")).is_some() {
                            "function"
                        } else {
                            "object"
                        }
                    } else {
                        "object"
                    }
                } else {
                    // All remaining pointer types (MapData, SetData, EphemeronTable, etc.)
                    "object"
                }
            }
        }
    }
}

impl Default for Value {
    fn default() -> Self {
        Self::undefined()
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.bits {
            TAG_UNDEFINED => write!(f, "undefined"),
            TAG_HOLE => write!(f, "<hole>"),
            TAG_NULL => write!(f, "null"),
            TAG_TRUE => write!(f, "true"),
            TAG_FALSE => write!(f, "false"),
            _ if self.is_int32() => write!(f, "{}", self.as_int32().unwrap()),
            _ if !self.is_nan_boxed() => write!(f, "{}", f64::from_bits(self.bits)),
            _ => {
                if let Some(s) = self.as_string() {
                    write!(f, "\"{}\"", s.as_str())
                } else if let Some(b) = self.as_bigint() {
                    write!(f, "{}n", b.value)
                } else if let Some(sym) = self.as_symbol() {
                    if let Some(desc) = &sym.description {
                        write!(f, "Symbol({})", desc)
                    } else {
                        write!(f, "Symbol()")
                    }
                } else if let Some(r) = self.as_regex() {
                    write!(f, "/{}/{}", r.pattern, r.flags)
                } else if let Some(ab) = self.as_array_buffer() {
                    write!(f, "ArrayBuffer({})", ab.byte_length())
                } else if let Some(ta) = self.as_typed_array() {
                    write!(f, "{}({})", ta.kind().name(), ta.length())
                } else if let Some(dv) = self.as_data_view() {
                    write!(f, "DataView({})", dv.byte_length())
                } else if let Some(sab) = self.as_shared_array_buffer() {
                    write!(f, "SharedArrayBuffer({})", sab.byte_length())
                } else if self.as_array().is_some() {
                    write!(f, "[object Array]")
                } else if self.as_function().is_some() {
                    write!(f, "[Function]")
                } else if self.is_native_function() {
                    write!(f, "[NativeFunction]")
                } else if self.is_promise() {
                    write!(f, "[object Promise]")
                } else if self.is_proxy() {
                    write!(f, "[object Proxy]")
                } else if self.is_generator() {
                    write!(f, "[object Generator]")
                } else if self.as_object().is_some() {
                    write!(f, "[object Object]")
                } else {
                    write!(f, "<heap:{:#x}>", self.bits)
                }
            }
        }
    }
}

impl PartialEq for Value {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        let self_bits = self.bits;
        let other_bits = other.bits;

        // NaN != NaN (IEEE 754)
        if self_bits == TAG_NAN || other_bits == TAG_NAN {
            return false;
        }

        // Fast path: same bits
        if self_bits == other_bits {
            return true;
        }

        // Int32 fast path: different tagged int32 values are never equal.
        // Avoids falling through to the generic number conversion path.
        let self_is_int32 = (self_bits & INT32_TAG_MASK) == TAG_INT32;
        let other_is_int32 = (other_bits & INT32_TAG_MASK) == TAG_INT32;
        if self_is_int32 && other_is_int32 {
            return false;
        }

        // Numbers: int32 and non-boxed f64 values.
        // TAG_NAN was already handled above, so numeric compare can be direct.
        let self_is_number = self_is_int32 || (self_bits & QUIET_NAN) != QUIET_NAN;
        let other_is_number = other_is_int32 || (other_bits & QUIET_NAN) != QUIET_NAN;
        if self_is_number && other_is_number {
            let a = if self_is_int32 {
                (self_bits as u32 as i32) as f64
            } else {
                f64::from_bits(self_bits)
            };
            let b = if other_is_int32 {
                (other_bits as u32 as i32) as f64
            } else {
                f64::from_bits(other_bits)
            };
            return a == b;
        }

        // Strings: compare contents
        if let (Some(a), Some(b)) = (self.as_string(), other.as_string()) {
            return a == b;
        }

        // BigInt equality
        if let (Some(a), Some(b)) = (self.as_bigint(), other.as_bigint()) {
            return a.value == b.value;
        }

        false
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl Value {
    /// Trace GC references in this value.
    ///
    /// Uses sub-tag + GcHeader tag for type discrimination, then traces
    /// the appropriate GC roots for each type.
    #[allow(unsafe_code)]
    pub fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        let tag16 = self.bits & TAG_MASK;
        match tag16 {
            TAG_PTR_STRING => {
                if let Some(hdr) = self.gc_header() {
                    tracer(hdr);
                }
            }
            TAG_PTR_OBJECT => {
                // Plain objects and arrays — trace the JsObject header
                if let Some(hdr) = self.gc_header() {
                    tracer(hdr);
                }
            }
            TAG_PTR_FUNCTION => {
                // Closure or NativeFunction — trace the wrapper's header (not the inner object)
                if let Some(hdr) = self.gc_header() {
                    tracer(hdr);
                }
            }
            TAG_PTR_OTHER => {
                let gc_tag = unsafe { self.gc_header_tag_from_bits() };
                use otter_vm_gc::object::tags as gc_tags;
                match gc_tag {
                    gc_tags::PROMISE => {
                        // Promise needs special trace_roots for reaction chains
                        if let Some(p) = self.as_promise() {
                            p.trace_roots(tracer);
                        }
                    }
                    gc_tags::TYPED_ARRAY => {
                        // TypedArray: trace both the object and its buffer
                        if let Some(ta) = self.as_typed_array() {
                            tracer(ta.object.header() as *const _);
                            tracer(ta.buffer().object.header() as *const _);
                        }
                    }
                    gc_tags::DATA_VIEW => {
                        // DataView: trace the buffer
                        if let Some(dv) = self.as_data_view() {
                            tracer(dv.buffer().object.header() as *const _);
                        }
                    }
                    gc_tags::REGEXP => {
                        // RegExp: trace inner object
                        if let Some(r) = self.as_regex() {
                            tracer(r.object.header() as *const _);
                        }
                    }
                    gc_tags::ARRAY_BUFFER => {
                        // ArrayBuffer: trace inner object
                        if let Some(ab) = self.as_array_buffer() {
                            tracer(ab.object.header() as *const _);
                        }
                    }
                    gc_tags::MAP_DATA => {
                        // MapData: trace all key/value pairs
                        if let Some(m) = self.as_map_data() {
                            otter_vm_gc::GcTraceable::trace(&*m, tracer);
                        }
                    }
                    gc_tags::SET_DATA => {
                        // SetData: trace all entries
                        if let Some(s) = self.as_set_data() {
                            otter_vm_gc::GcTraceable::trace(&*s, tracer);
                        }
                    }
                    gc_tags::FINALIZATION_REGISTRY => {
                        // FinalizationRegistry: trace callback + held values
                        if let Some(r) = self.as_finalization_registry() {
                            otter_vm_gc::GcTraceable::trace(&*r, tracer);
                        }
                    }
                    gc_tags::ACCESSOR_PAIR => {
                        // AccessorPair: trace getter + setter Values
                        if let Some(pair) = self.as_accessor_pair() {
                            otter_vm_gc::GcTraceable::trace(&*pair, tracer);
                        }
                    }
                    _ => {
                        // Symbol, BigInt, Generator, Proxy, SharedArrayBuffer,
                        // EphemeronTable, WeakRef, Temporal — trace just the header
                        if let Some(hdr) = self.gc_header() {
                            tracer(hdr);
                        }
                    }
                }
            }
            _ => {
                // Not a pointer type (int32, f64, bool, null, undefined) — nothing to trace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_undefined() {
        let v = Value::undefined();
        assert!(v.is_undefined());
        assert!(!v.to_boolean());
        assert_eq!(v.type_of(), "undefined");
    }

    #[test]
    fn test_null() {
        let v = Value::null();
        assert!(v.is_null());
        assert!(v.is_nullish());
        assert!(!v.to_boolean());
        assert_eq!(v.type_of(), "object");
    }

    #[test]
    fn test_boolean() {
        let t = Value::boolean(true);
        let f = Value::boolean(false);

        assert!(t.is_boolean());
        assert!(f.is_boolean());
        assert!(t.to_boolean());
        assert!(!f.to_boolean());
        assert_eq!(t.type_of(), "boolean");
    }

    #[test]
    fn test_int32() {
        let v = Value::int32(42);
        assert!(v.is_int32());
        assert!(v.is_number());
        assert_eq!(v.as_int32(), Some(42));
        assert_eq!(v.as_number(), Some(42.0));
        assert_eq!(v.type_of(), "number");
    }

    #[test]
    fn test_number() {
        let v = Value::number(3.15);
        assert!(v.is_number());
        assert!(!v.is_int32()); // Has fractional part
        assert_eq!(v.as_number(), Some(3.15));
    }

    #[test]
    fn test_nan() {
        // NaN via number()
        let v = Value::number(f64::NAN);
        assert!(v.is_nan());
        assert!(v.is_number());
        assert!(!v.is_undefined()); // NaN is distinct from undefined
        assert!(v.as_number().unwrap().is_nan());
        assert_eq!(v.type_of(), "number");

        // NaN via nan()
        let v2 = Value::nan();
        assert!(v2.is_nan());
        assert!(v2.is_number());

        // NaN != NaN (per IEEE 754)
        assert_ne!(v, v2); // Our PartialEq uses a == b which returns false for NaN
    }

    #[test]
    fn test_hole() {
        let v = Value::hole();
        assert!(v.is_hole());
        assert!(!v.is_undefined());
        assert!(!v.is_null());
        assert!(!v.is_nullish());
        assert!(!v.to_boolean());
        assert_eq!(v.type_of(), "undefined");
    }

    #[test]
    fn test_value_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Value>();
    }

    #[test]
    fn test_native_function_identity() {
        fn noop(
            _this: &Value,
            _args: &[Value],
            _cx: &mut crate::context::NativeContext<'_>,
        ) -> Result<Value, crate::error::VmError> {
            Ok(Value::undefined())
        }

        let _rt = crate::runtime::VmRuntime::new();
        let mm = _rt.memory_manager().clone();
        let v1 = Value::native_function(noop, mm.clone());
        let v2 = Value::native_function(noop, mm.clone());

        assert_eq!(v1, v1.clone());
        assert_ne!(v1, v2);
    }

    #[test]
    fn test_from_jit_bits_rejects_pointer_tag() {
        // All 4 pointer sub-tags should be rejected
        assert!(Value::from_jit_bits(0x7FFC_0000_0000_0001).is_none()); // Object
        assert!(Value::from_jit_bits(0x7FFD_0000_0000_0001).is_none()); // String
        assert!(Value::from_jit_bits(0x7FFE_0000_0000_0001).is_none()); // Function
        assert!(Value::from_jit_bits(0x7FFF_0000_0000_0001).is_none()); // Other

        let boxed_int = Value::int32(7).bits;
        let restored = Value::from_jit_bits(boxed_int).expect("int32 bits should be accepted");
        assert_eq!(restored.as_int32(), Some(7));
    }
}
