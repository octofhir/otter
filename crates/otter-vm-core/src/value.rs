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
use crate::object::JsObject;
use crate::promise::JsPromise;
use crate::proxy::JsProxy;
use crate::regexp::JsRegExp;
use crate::shared_buffer::SharedArrayBuffer;
use crate::string::JsString;
use crate::typed_array::JsTypedArray;
use std::cell::RefCell;
use std::sync::Arc;

/// Heap-allocated cell for mutable upvalues (closures)
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
#[derive(Clone)]
pub struct UpvalueCell(Arc<RefCell<Value>>);

// SAFETY: UpvalueCell is only accessed from the single VM thread.
// Thread confinement is enforced at the VmRuntime/VmContext level.
unsafe impl Send for UpvalueCell {}
unsafe impl Sync for UpvalueCell {}

impl UpvalueCell {
    /// Create a new upvalue cell with the given value
    pub fn new(value: Value) -> Self {
        Self(Arc::new(RefCell::new(value)))
    }

    /// Get the current value from the cell
    pub fn get(&self) -> Value {
        self.0.borrow().clone()
    }

    /// Set a new value in the cell
    pub fn set(&self, value: Value) {
        *self.0.borrow_mut() = value;
    }
}

impl std::fmt::Debug for UpvalueCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UpvalueCell({:?})", *self.0.borrow())
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
const TAG_POINTER: u64 = 0x7FFC_0000_0000_0000;

/// A JavaScript value using NaN-boxing for efficient storage
///
/// This type is `Send + Sync` because all heap-allocated data is behind `Arc`.
#[derive(Clone)]
pub struct Value {
    bits: u64,
    /// Heap reference to prevent GC while value is alive
    /// This is Some only for pointer types (Object, String, etc.)
    heap_ref: Option<HeapRef>,
}

// SAFETY: Value contains only u64 bits and Arc (which is Send+Sync)
unsafe impl Send for Value {}
unsafe impl Sync for Value {}

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

/// Reference to heap-allocated data
#[derive(Clone)]
pub enum HeapRef {
    /// String value (GC-managed)
    String(GcRef<JsString>),
    /// Object value (GC-managed)
    Object(GcRef<JsObject>),
    /// Array value (stored as Object internally, GC-managed)
    Array(GcRef<JsObject>),
    /// Function closure
    Function(GcRef<Closure>),
    /// Symbol
    Symbol(GcRef<Symbol>),
    /// BigInt
    BigInt(GcRef<BigInt>),
    /// Promise
    Promise(GcRef<JsPromise>),
    /// Proxy object
    Proxy(GcRef<JsProxy>),
    /// Generator object
    Generator(GcRef<JsGenerator>),
    /// ArrayBuffer (raw binary data buffer)
    ArrayBuffer(GcRef<JsArrayBuffer>),
    /// TypedArray (view over ArrayBuffer)
    TypedArray(GcRef<JsTypedArray>),
    /// DataView (arbitrary byte-order access to ArrayBuffer)
    DataView(GcRef<JsDataView>),
    /// SharedArrayBuffer (can be shared between workers)
    SharedArrayBuffer(GcRef<SharedArrayBuffer>),
    /// Native function (implemented in Rust)
    NativeFunction(GcRef<NativeFunctionObject>),
    /// RegExp
    RegExp(GcRef<JsRegExp>),
    /// Map internal data
    MapData(GcRef<MapData>),
    /// Set internal data
    SetData(GcRef<SetData>),
    /// Ephemeron table for WeakMap/WeakSet
    EphemeronTable(GcRef<otter_vm_gc::EphemeronTable>),
}

impl std::fmt::Debug for HeapRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeapRef::String(s) => f.debug_tuple("String").field(s).finish(),
            HeapRef::Object(o) => f.debug_tuple("Object").field(o).finish(),
            HeapRef::Array(a) => f.debug_tuple("Array").field(a).finish(),
            HeapRef::Function(c) => f.debug_tuple("Function").field(c).finish(),
            HeapRef::Symbol(s) => f.debug_tuple("Symbol").field(s).finish(),
            HeapRef::BigInt(b) => f.debug_tuple("BigInt").field(b).finish(),
            HeapRef::Promise(p) => f.debug_tuple("Promise").field(p).finish(),
            HeapRef::Proxy(p) => f.debug_tuple("Proxy").field(p).finish(),
            HeapRef::Generator(g) => f.debug_tuple("Generator").field(g).finish(),
            HeapRef::ArrayBuffer(a) => f.debug_tuple("ArrayBuffer").field(a).finish(),
            HeapRef::TypedArray(t) => f.debug_tuple("TypedArray").field(t).finish(),
            HeapRef::DataView(d) => f.debug_tuple("DataView").field(d).finish(),
            HeapRef::SharedArrayBuffer(s) => f.debug_tuple("SharedArrayBuffer").field(s).finish(),
            HeapRef::NativeFunction(_) => f.debug_tuple("NativeFunction").finish(),
            HeapRef::RegExp(r) => f.debug_tuple("RegExp").field(r).finish(),
            HeapRef::MapData(m) => f.debug_tuple("MapData").field(m).finish(),
            HeapRef::SetData(s) => f.debug_tuple("SetData").field(s).finish(),
            HeapRef::EphemeronTable(e) => f.debug_tuple("EphemeronTable").field(e).finish(),
        }
    }
}

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

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace function object
        tracer(self.object.header() as *const _);

        if let Some(home) = &self.home_object {
            tracer(home.header() as *const _);
        }

        // Each UpvalueCell contains Arc<RefCell<Value>>, trace the Value inside
        for upvalue in &self.upvalues {
            let value = upvalue.get(); // Locks and clones the Value
            value.trace(tracer);
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
}

impl otter_vm_gc::GcTraceable for NativeFunctionObject {
    const NEEDS_TRACE: bool = true;

    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the attached object
        tracer(self.object.header() as *const _);

        // NativeFn is Arc<dyn Fn>, which is opaque to GC
        // Any Value references are passed through arguments, not captured in the closure
        // Native functions are created by Rust code and don't capture GC-managed values
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
            heap_ref: None,
        }
    }

    /// Create null value
    #[inline]
    pub const fn null() -> Self {
        Self {
            bits: TAG_NULL,
            heap_ref: None,
        }
    }

    /// Create boolean value
    #[inline]
    pub const fn boolean(b: bool) -> Self {
        Self {
            bits: if b { TAG_TRUE } else { TAG_FALSE },
            heap_ref: None,
        }
    }

    /// Create an array hole sentinel.
    ///
    /// Holes represent absent elements in sparse arrays (e.g. `[1,,3]` or after
    /// `delete arr[i]`). They are never user-visible: `get()` converts them to
    /// `undefined`, and `has_own()` / `in` treats them as absent.
    #[inline]
    pub const fn hole() -> Self {
        Self {
            bits: TAG_HOLE,
            heap_ref: None,
        }
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
            heap_ref: None,
        }
    }

    /// Create number (f64) value
    #[inline]
    pub fn number(n: f64) -> Self {
        // Handle NaN specially to avoid collision with undefined
        if n.is_nan() {
            return Self {
                bits: TAG_NAN,
                heap_ref: None,
            };
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

        Self {
            bits: n.to_bits(),
            heap_ref: None,
        }
    }

    /// Create NaN value explicitly
    #[inline]
    pub const fn nan() -> Self {
        Self {
            bits: TAG_NAN,
            heap_ref: None,
        }
    }

    /// Create string value (GC-managed)
    pub fn string(s: GcRef<JsString>) -> Self {
        // Store pointer address in NaN-boxed format
        let ptr = s.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::String(s)),
        }
    }

    /// Create object value (GC-managed)
    pub fn object(obj: GcRef<JsObject>) -> Self {
        let ptr = obj.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Object(obj)),
        }
    }

    /// Create function closure value
    pub fn function(closure: GcRef<Closure>) -> Self {
        let ptr = closure.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Function(closure)),
        }
    }

    /// Create promise value
    pub fn promise(promise: GcRef<JsPromise>) -> Self {
        let ptr = promise.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Promise(promise)),
        }
    }

    pub fn regex(regex: GcRef<JsRegExp>) -> Self {
        let ptr = regex.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::RegExp(regex)),
        }
    }

    /// Create proxy value
    pub fn proxy(proxy: GcRef<JsProxy>) -> Self {
        let ptr = proxy.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Proxy(proxy)),
        }
    }

    /// Create generator value
    pub fn generator(generator: GcRef<JsGenerator>) -> Self {
        let ptr = generator.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Generator(generator)),
        }
    }

    /// Create ArrayBuffer value
    pub fn array_buffer(ab: GcRef<JsArrayBuffer>) -> Self {
        let ptr = ab.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::ArrayBuffer(ab)),
        }
    }

    /// Create TypedArray value
    pub fn typed_array(ta: GcRef<JsTypedArray>) -> Self {
        let ptr = ta.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::TypedArray(ta)),
        }
    }

    /// Create DataView value
    pub fn data_view(dv: GcRef<JsDataView>) -> Self {
        let ptr = dv.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::DataView(dv)),
        }
    }

    /// Create SharedArrayBuffer value
    pub fn shared_array_buffer(sab: GcRef<SharedArrayBuffer>) -> Self {
        let ptr = sab.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::SharedArrayBuffer(sab)),
        }
    }

    /// Create Map internal data value
    pub fn map_data(data: GcRef<MapData>) -> Self {
        let ptr = data.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::MapData(data)),
        }
    }

    /// Create Set internal data value
    pub fn set_data(data: GcRef<SetData>) -> Self {
        let ptr = data.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::SetData(data)),
        }
    }

    /// Create ephemeron table value (for WeakMap/WeakSet)
    pub fn ephemeron_table(table: GcRef<otter_vm_gc::EphemeronTable>) -> Self {
        let ptr = table.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::EphemeronTable(table)),
        }
    }

    /// Create array value (GC-managed)
    pub fn array(arr: GcRef<JsObject>) -> Self {
        let ptr = arr.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Array(arr)),
        }
    }

    /// Create BigInt value
    pub fn bigint(value: String) -> Self {
        let bi = GcRef::new(BigInt { value });
        let ptr = bi.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::BigInt(bi)),
        }
    }

    /// Create Symbol value
    pub fn symbol(sym: GcRef<Symbol>) -> Self {
        let ptr = sym.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Symbol(sym)),
        }
    }

    /// Create native function value
    pub fn native_function<F>(f: F, memory_manager: Arc<crate::memory::MemoryManager>) -> Self
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
        let object = GcRef::new(JsObject::new(Value::null(), memory_manager));
        let native = GcRef::new(NativeFunctionObject { func, object });
        let ptr = native.as_ptr() as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::NativeFunction(native)),
        }
    }

    /// Create a native function value with a specific [[Prototype]].
    ///
    /// Per ES2023 §10.3.1, built-in function objects must have
    /// `%Function.prototype%` as their `[[Prototype]]`. Use this
    /// constructor when `Function.prototype` is already available.
    pub fn native_function_with_proto<F>(
        f: F,
        memory_manager: Arc<crate::memory::MemoryManager>,
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
        let object = GcRef::new(JsObject::new(Value::object(prototype), memory_manager));
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
        if let Some(realm_id) = prototype
            .get(&crate::object::PropertyKey::string("__realm_id__"))
            .and_then(|v| v.as_int32())
        {
            object.define_property(
                crate::object::PropertyKey::string("__realm_id__"),
                crate::object::PropertyDescriptor::builtin_data(Value::int32(realm_id)),
            );
        }
        let native = GcRef::new(NativeFunctionObject { func, object });
        Self {
            bits: TAG_POINTER | (native.as_ptr() as u64 & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::NativeFunction(native)),
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
        let native = GcRef::new(NativeFunctionObject { func, object });
        Self {
            bits: TAG_POINTER | (native.as_ptr() as u64 & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::NativeFunction(native)),
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
    #[inline]
    pub fn is_int32(&self) -> bool {
        (self.bits & 0xFFFF_FFFF_0000_0000) == TAG_INT32
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
    #[inline]
    pub fn is_string(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::String(_)))
    }

    /// Check if value is an object (includes functions, arrays, regexps, etc.)
    #[inline]
    pub fn is_object(&self) -> bool {
        matches!(
            &self.heap_ref,
            Some(HeapRef::Object(_))
                | Some(HeapRef::Array(_))
                | Some(HeapRef::RegExp(_))
                | Some(HeapRef::Function(_))
                | Some(HeapRef::NativeFunction(_))
                | Some(HeapRef::Promise(_))
                | Some(HeapRef::Proxy(_))
                | Some(HeapRef::Generator(_))
        )
    }

    /// Check if value is a function (includes native functions)
    #[inline]
    pub fn is_function(&self) -> bool {
        matches!(
            &self.heap_ref,
            Some(HeapRef::Function(_)) | Some(HeapRef::NativeFunction(_))
        )
    }

    /// Check if value is a promise
    #[inline]
    pub fn is_promise(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::Promise(_)))
    }

    /// Check if value is a proxy
    #[inline]
    pub fn is_proxy(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::Proxy(_)))
    }

    /// Check if value is a generator
    #[inline]
    pub fn is_generator(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::Generator(_)))
    }

    /// Check if value is an ArrayBuffer
    #[inline]
    pub fn is_array_buffer(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::ArrayBuffer(_)))
    }

    /// Check if value is a TypedArray
    #[inline]
    pub fn is_typed_array(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::TypedArray(_)))
    }

    /// Check if value is a DataView
    #[inline]
    pub fn is_data_view(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::DataView(_)))
    }

    /// Check if value is a SharedArrayBuffer
    #[inline]
    pub fn is_shared_array_buffer(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::SharedArrayBuffer(_)))
    }

    /// Check if value is a native function
    #[inline]
    pub fn is_native_function(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::NativeFunction(_)))
    }

    /// Check if value is callable (function, native function, or bound function)
    #[inline]
    pub fn is_callable(&self) -> bool {
        if self.is_function() || self.is_native_function() {
            return true;
        }
        if let Some(proxy) = self.as_proxy() {
            if let Some(target) = proxy.target() {
                return target.is_callable();
            }
            return proxy.target_raw().is_callable();
        }
        // Bound functions are plain objects with __boundFunction__ property
        if let Some(obj) = self.as_object() {
            if obj
                .get(&crate::object::PropertyKey::string("__boundFunction__"))
                .is_some()
            {
                return true;
            }
        }
        false
    }

    /// Check if value is a symbol
    #[inline]
    pub fn is_symbol(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::Symbol(_)))
    }

    /// Check if value is a BigInt
    #[inline]
    pub fn is_bigint(&self) -> bool {
        matches!(&self.heap_ref, Some(HeapRef::BigInt(_)))
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
    pub fn as_string(&self) -> Option<GcRef<JsString>> {
        match &self.heap_ref {
            Some(HeapRef::String(s)) => Some(*s),
            _ => None,
        }
    }

    /// Get as object (GC-managed)
    pub fn as_object(&self) -> Option<GcRef<JsObject>> {
        match &self.heap_ref {
            Some(HeapRef::Object(o)) => Some(*o),
            Some(HeapRef::Array(a)) => Some(*a),
            Some(HeapRef::Function(f)) => Some(f.object),
            Some(HeapRef::NativeFunction(n)) => Some(n.object),
            Some(HeapRef::Generator(g)) => Some(g.object),
            Some(HeapRef::RegExp(r)) => Some(r.object),
            Some(HeapRef::ArrayBuffer(ab)) => Some(ab.object),
            Some(HeapRef::TypedArray(ta)) => Some(ta.object),
            _ => None,
        }
    }

    /// Get as array object (GC-managed)
    pub fn as_array(&self) -> Option<GcRef<JsObject>> {
        match &self.heap_ref {
            Some(HeapRef::Array(a)) => Some(*a),
            _ => None,
        }
    }

    /// Get as function closure
    pub fn as_function(&self) -> Option<GcRef<Closure>> {
        match &self.heap_ref {
            Some(HeapRef::Function(f)) => Some(*f),
            _ => None,
        }
    }

    /// Get the inner `JsObject` attached to a function (closure or native).
    /// This is the object that holds properties like `.prototype`, `.name`, `.length`
    /// and carries the `[[Prototype]]` internal slot (should be `Function.prototype`).
    pub fn function_inner_object(&self) -> Option<GcRef<JsObject>> {
        match &self.heap_ref {
            Some(HeapRef::Function(f)) => Some(f.object),
            Some(HeapRef::NativeFunction(n)) => Some(n.object),
            _ => None,
        }
    }

    /// Get as native function
    pub fn as_native_function(&self) -> Option<&NativeFn> {
        match &self.heap_ref {
            Some(HeapRef::NativeFunction(f)) => Some(&f.func),
            _ => None,
        }
    }

    /// Get the properties object of a native function, for setting `name`, `length`, etc.
    /// Returns `None` if the value is not a `NativeFunction`.
    pub fn native_function_object(&self) -> Option<GcRef<JsObject>> {
        match &self.heap_ref {
            Some(HeapRef::NativeFunction(f)) => Some(f.object),
            _ => None,
        }
    }

    /// Get as promise
    pub fn as_promise(&self) -> Option<GcRef<JsPromise>> {
        match &self.heap_ref {
            Some(HeapRef::Promise(p)) => Some(*p),
            _ => None,
        }
    }

    /// Get as proxy
    pub fn as_proxy(&self) -> Option<GcRef<JsProxy>> {
        match &self.heap_ref {
            Some(HeapRef::Proxy(p)) => Some(*p),
            _ => None,
        }
    }

    /// Get as generator
    pub fn as_generator(&self) -> Option<GcRef<JsGenerator>> {
        match &self.heap_ref {
            Some(HeapRef::Generator(g)) => Some(*g),
            _ => None,
        }
    }

    /// Get as regex
    pub fn as_regex(&self) -> Option<GcRef<JsRegExp>> {
        match &self.heap_ref {
            Some(HeapRef::RegExp(r)) => Some(*r),
            _ => None,
        }
    }

    /// Get as ArrayBuffer
    pub fn as_array_buffer(&self) -> Option<GcRef<JsArrayBuffer>> {
        match &self.heap_ref {
            Some(HeapRef::ArrayBuffer(ab)) => Some(*ab),
            _ => None,
        }
    }

    /// Get as TypedArray
    pub fn as_typed_array(&self) -> Option<GcRef<JsTypedArray>> {
        match &self.heap_ref {
            Some(HeapRef::TypedArray(ta)) => Some(*ta),
            _ => None,
        }
    }

    /// Get as DataView
    pub fn as_data_view(&self) -> Option<GcRef<JsDataView>> {
        match &self.heap_ref {
            Some(HeapRef::DataView(dv)) => Some(*dv),
            _ => None,
        }
    }

    /// Get as SharedArrayBuffer
    pub fn as_shared_array_buffer(&self) -> Option<GcRef<SharedArrayBuffer>> {
        match &self.heap_ref {
            Some(HeapRef::SharedArrayBuffer(sab)) => Some(*sab),
            _ => None,
        }
    }

    /// Get as symbol
    pub fn as_symbol(&self) -> Option<GcRef<Symbol>> {
        match &self.heap_ref {
            Some(HeapRef::Symbol(s)) => Some(*s),
            _ => None,
        }
    }

    /// Get as Map internal data
    pub fn as_map_data(&self) -> Option<GcRef<MapData>> {
        match &self.heap_ref {
            Some(HeapRef::MapData(m)) => Some(*m),
            _ => None,
        }
    }

    /// Get as Set internal data
    pub fn as_set_data(&self) -> Option<GcRef<SetData>> {
        match &self.heap_ref {
            Some(HeapRef::SetData(s)) => Some(*s),
            _ => None,
        }
    }

    /// Get as ephemeron table
    pub fn as_ephemeron_table(&self) -> Option<GcRef<otter_vm_gc::EphemeronTable>> {
        match &self.heap_ref {
            Some(HeapRef::EphemeronTable(e)) => Some(*e),
            _ => None,
        }
    }

    /// Get the heap reference (for structured clone)
    #[doc(hidden)]
    pub fn heap_ref(&self) -> &Option<HeapRef> {
        &self.heap_ref
    }

    /// Get the GC header pointer for this value (if it's a GC-managed type)
    ///
    /// Returns the pointer to the GcHeader if this value contains a GC-managed
    /// object (String or Object), otherwise returns None.
    pub fn gc_header(&self) -> Option<*const otter_vm_gc::GcHeader> {
        match &self.heap_ref {
            Some(HeapRef::String(s)) => Some(s.header() as *const _),
            Some(HeapRef::Object(o)) => Some(o.header() as *const _),
            Some(HeapRef::Array(a)) => Some(a.header() as *const _),
            Some(HeapRef::Function(f)) => Some(f.object.header() as *const _),
            Some(HeapRef::NativeFunction(n)) => Some(n.object.header() as *const _),
            Some(HeapRef::RegExp(r)) => Some(r.object.header() as *const _),
            Some(HeapRef::Generator(g)) => Some(g.object.header() as *const _),
            Some(HeapRef::Symbol(s)) => Some(s.header() as *const _),
            Some(HeapRef::BigInt(b)) => Some(b.header() as *const _),
            Some(HeapRef::Promise(p)) => Some(p.header() as *const _),
            Some(HeapRef::Proxy(p)) => Some(p.header() as *const _),
            Some(HeapRef::ArrayBuffer(a)) => Some(a.header() as *const _),
            Some(HeapRef::TypedArray(t)) => Some(t.header() as *const _),
            Some(HeapRef::DataView(d)) => Some(d.header() as *const _),
            Some(HeapRef::SharedArrayBuffer(s)) => Some(s.header() as *const _),
            Some(HeapRef::MapData(m)) => Some(m.header() as *const _),
            Some(HeapRef::SetData(s)) => Some(s.header() as *const _),
            Some(HeapRef::EphemeronTable(e)) => Some(e.header() as *const _),
            None => None,
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
                if let Some(HeapRef::BigInt(b)) = &self.heap_ref {
                    return b.value != "0";
                }
                // Strings: empty string is false
                if let Some(s) = self.as_string() {
                    !s.is_empty()
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
            _ => match &self.heap_ref {
                Some(HeapRef::String(_)) => "string",
                Some(HeapRef::Function(_) | HeapRef::NativeFunction(_)) => "function",
                Some(HeapRef::Symbol(_)) => "symbol",
                Some(HeapRef::BigInt(_)) => "bigint",
                Some(HeapRef::RegExp(_)) => "object",
                Some(HeapRef::Object(obj)) => {
                    // Check if it's a bound function (has __boundFunction__ property)
                    if obj.get(&PropertyKey::string("__boundFunction__")).is_some() {
                        "function"
                    } else {
                        "object"
                    }
                }
                Some(
                    HeapRef::Array(_)
                    | HeapRef::Promise(_)
                    | HeapRef::Generator(_)
                    | HeapRef::ArrayBuffer(_)
                    | HeapRef::TypedArray(_)
                    | HeapRef::DataView(_)
                    | HeapRef::SharedArrayBuffer(_)
                    | HeapRef::MapData(_)
                    | HeapRef::SetData(_)
                    | HeapRef::EphemeronTable(_),
                ) => "object",
                Some(HeapRef::Proxy(_)) => {
                    if self.is_callable() {
                        "function"
                    } else {
                        "object"
                    }
                }
                None => "undefined", // Should not happen
            },
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
            _ => match &self.heap_ref {
                Some(HeapRef::String(s)) => write!(f, "\"{}\"", s.as_str()),
                Some(HeapRef::Object(_)) => write!(f, "[object Object]"),
                Some(HeapRef::Array(_)) => write!(f, "[object Array]"),
                Some(HeapRef::Function(_)) => write!(f, "[Function]"),
                Some(HeapRef::NativeFunction(_)) => write!(f, "[NativeFunction]"),
                Some(HeapRef::Promise(_)) => write!(f, "[object Promise]"),
                Some(HeapRef::Proxy(_)) => write!(f, "[object Proxy]"),
                Some(HeapRef::RegExp(r)) => write!(f, "/{}/{}", r.pattern, r.flags),
                Some(HeapRef::Generator(_)) => write!(f, "[object Generator]"),
                Some(HeapRef::Symbol(s)) => {
                    if let Some(desc) = &s.description {
                        write!(f, "Symbol({})", desc)
                    } else {
                        write!(f, "Symbol()")
                    }
                }
                Some(HeapRef::BigInt(b)) => write!(f, "{}n", b.value),
                Some(HeapRef::ArrayBuffer(ab)) => {
                    write!(f, "ArrayBuffer({})", ab.byte_length())
                }
                Some(HeapRef::TypedArray(ta)) => {
                    write!(f, "{}({})", ta.kind().name(), ta.length())
                }
                Some(HeapRef::DataView(dv)) => {
                    write!(f, "DataView({})", dv.byte_length())
                }
                Some(HeapRef::SharedArrayBuffer(sab)) => {
                    write!(f, "SharedArrayBuffer({})", sab.byte_length())
                }
                Some(HeapRef::MapData(m)) => write!(f, "[MapData {:?}]", m),
                Some(HeapRef::SetData(s)) => write!(f, "[SetData {:?}]", s),
                Some(HeapRef::EphemeronTable(e)) => write!(f, "[EphemeronTable {:?}]", e),
                None => write!(f, "<unknown>"),
            },
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        // NaN != NaN (IEEE 754)
        if self.bits == TAG_NAN || other.bits == TAG_NAN {
            return false;
        }

        // Fast path: same bits
        if self.bits == other.bits {
            return true;
        }

        // Numbers: need special handling for NaN
        if self.is_number() && other.is_number() {
            let a = self.as_number().unwrap();
            let b = other.as_number().unwrap();
            return a == b; // NaN != NaN is correct
        }

        // Strings: compare contents
        if let (Some(a), Some(b)) = (self.as_string(), other.as_string()) {
            return a == b;
        }

        // BigInt equality
        if let (Some(HeapRef::BigInt(a)), Some(HeapRef::BigInt(b))) =
            (self.heap_ref(), other.heap_ref())
        {
            return a.value == b.value;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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

        let mm = Arc::new(crate::memory::MemoryManager::test());
        let v1 = Value::native_function(noop, mm.clone());
        let v2 = Value::native_function(noop, mm.clone());

        assert_eq!(v1, v1.clone());
        assert_ne!(v1, v2);
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl Value {
    /// Trace GC references in this value
    pub fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        if let Some(heap_ref) = &self.heap_ref {
            match heap_ref {
                HeapRef::String(s) => {
                    tracer(s.header() as *const _);
                }
                HeapRef::Object(o) | HeapRef::Array(o) => {
                    tracer(o.header() as *const _);
                }
                HeapRef::Function(f) => {
                    tracer(f.header() as *const _);
                }
                HeapRef::NativeFunction(n) => {
                    tracer(n.header() as *const _);
                }
                HeapRef::RegExp(r) => {
                    tracer(r.object.header() as *const _);
                }
                HeapRef::Generator(g) => {
                    tracer(g.header() as *const _);
                }
                HeapRef::ArrayBuffer(ab) => {
                    tracer(ab.object.header() as *const _);
                }
                HeapRef::TypedArray(ta) => {
                    tracer(ta.object.header() as *const _);
                    tracer(ta.buffer().object.header() as *const _);
                }
                HeapRef::DataView(dv) => {
                    tracer(dv.buffer().object.header() as *const _);
                }
                HeapRef::Promise(p) => {
                    p.trace_roots(tracer);
                }
                HeapRef::Proxy(p) => {
                    p.target.trace(tracer);
                    p.handler.trace(tracer);
                }
                HeapRef::Symbol(s) => {
                    tracer(s.header() as *const _);
                }
                HeapRef::BigInt(b) => {
                    tracer(b.header() as *const _);
                }
                HeapRef::SharedArrayBuffer(sab) => {
                    tracer(sab.header() as *const _);
                }
                HeapRef::MapData(m) => {
                    otter_vm_gc::GcTraceable::trace(&**m, tracer);
                }
                HeapRef::SetData(s) => {
                    otter_vm_gc::GcTraceable::trace(&**s, tracer);
                }
                // Other types use Arc, not GcRef, so no GC tracing needed
                _ => {}
            }
        }
    }
}
