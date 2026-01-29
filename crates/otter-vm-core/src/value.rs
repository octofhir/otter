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
use crate::object::JsObject;
use crate::promise::JsPromise;
use crate::proxy::JsProxy;
use crate::regexp::JsRegExp;
use crate::shared_buffer::SharedArrayBuffer;
use crate::string::JsString;
use crate::typed_array::JsTypedArray;
use parking_lot::Mutex;
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
pub struct UpvalueCell(Arc<Mutex<Value>>);

impl UpvalueCell {
    /// Create a new upvalue cell with the given value
    pub fn new(value: Value) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }

    /// Get the current value from the cell
    pub fn get(&self) -> Value {
        self.0.lock().clone()
    }

    /// Set a new value in the cell
    pub fn set(&self, value: Value) {
        *self.0.lock() = value;
    }
}

impl std::fmt::Debug for UpvalueCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UpvalueCell({:?})", *self.0.lock())
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

/// Native function handler type
pub type NativeFn =
    Arc<dyn Fn(&[Value], Arc<crate::memory::MemoryManager>) -> Result<Value, String> + Send + Sync>;

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
    Function(Arc<Closure>),
    /// Symbol
    Symbol(Arc<Symbol>),
    /// BigInt
    BigInt(Arc<BigInt>),
    /// Promise
    Promise(Arc<JsPromise>),
    /// Proxy object
    Proxy(Arc<JsProxy>),
    /// Generator object
    Generator(Arc<JsGenerator>),
    /// ArrayBuffer (raw binary data buffer)
    ArrayBuffer(Arc<JsArrayBuffer>),
    /// TypedArray (view over ArrayBuffer)
    TypedArray(Arc<JsTypedArray>),
    /// DataView (arbitrary byte-order access to ArrayBuffer)
    DataView(Arc<JsDataView>),
    /// SharedArrayBuffer (can be shared between workers)
    SharedArrayBuffer(Arc<SharedArrayBuffer>),
    /// Native function (implemented in Rust)
    NativeFunction(Arc<NativeFunctionObject>),
    /// RegExp
    RegExp(Arc<JsRegExp>),
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
}

/// A native function with an attached object for properties.
#[derive(Clone)]
pub struct NativeFunctionObject {
    /// The native function handler
    pub func: NativeFn,
    /// Attached object for properties (GC-managed)
    pub object: GcRef<JsObject>,
}

/// A JavaScript Symbol
#[derive(Debug)]
pub struct Symbol {
    /// Symbol description
    pub description: Option<String>,
    /// Unique ID
    pub id: u64,
}

/// A JavaScript BigInt (arbitrary precision integer)
#[derive(Debug)]
pub struct BigInt {
    /// String representation (for now)
    pub value: String,
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
    pub fn function(closure: Arc<Closure>) -> Self {
        let ptr = Arc::as_ptr(&closure) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Function(closure)),
        }
    }

    /// Create promise value
    pub fn promise(promise: Arc<JsPromise>) -> Self {
        let ptr = Arc::as_ptr(&promise) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Promise(promise)),
        }
    }

    pub fn regex(regex: Arc<JsRegExp>) -> Self {
        let ptr = Arc::as_ptr(&regex) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::RegExp(regex)),
        }
    }

    /// Create proxy value
    pub fn proxy(proxy: Arc<JsProxy>) -> Self {
        let ptr = Arc::as_ptr(&proxy) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Proxy(proxy)),
        }
    }

    /// Create generator value
    pub fn generator(generator: Arc<JsGenerator>) -> Self {
        let ptr = Arc::as_ptr(&generator) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Generator(generator)),
        }
    }

    /// Create ArrayBuffer value
    pub fn array_buffer(ab: Arc<JsArrayBuffer>) -> Self {
        let ptr = Arc::as_ptr(&ab) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::ArrayBuffer(ab)),
        }
    }

    /// Create TypedArray value
    pub fn typed_array(ta: Arc<JsTypedArray>) -> Self {
        let ptr = Arc::as_ptr(&ta) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::TypedArray(ta)),
        }
    }

    /// Create DataView value
    pub fn data_view(dv: Arc<JsDataView>) -> Self {
        let ptr = Arc::as_ptr(&dv) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::DataView(dv)),
        }
    }

    /// Create SharedArrayBuffer value
    pub fn shared_array_buffer(sab: Arc<SharedArrayBuffer>) -> Self {
        let ptr = Arc::as_ptr(&sab) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::SharedArrayBuffer(sab)),
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
        let bi = Arc::new(BigInt { value });
        let ptr = Arc::as_ptr(&bi) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::BigInt(bi)),
        }
    }

    /// Create Symbol value
    pub fn symbol(sym: Arc<Symbol>) -> Self {
        let ptr = Arc::as_ptr(&sym) as u64;
        Self {
            bits: TAG_POINTER | (ptr & PAYLOAD_MASK),
            heap_ref: Some(HeapRef::Symbol(sym)),
        }
    }

    /// Create native function value
    pub fn native_function<F>(f: F, memory_manager: Arc<crate::memory::MemoryManager>) -> Self
    where
        F: Fn(&[Value], Arc<crate::memory::MemoryManager>) -> Result<Value, String>
            + Send
            + Sync
            + 'static,
    {
        let func: NativeFn = Arc::new(f);
        let object = GcRef::new(JsObject::new(None, memory_manager));
        let native = Arc::new(NativeFunctionObject { func, object });
        // Use a dummy pointer for NaN-boxing (the actual function is in heap_ref)
        Self {
            bits: TAG_POINTER,
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
        F: Fn(&[Value], Arc<crate::memory::MemoryManager>) -> Result<Value, String>
            + Send
            + Sync
            + 'static,
    {
        let func: NativeFn = Arc::new(f);
        let object = GcRef::new(JsObject::new(Some(prototype), memory_manager));
        let native = Arc::new(NativeFunctionObject { func, object });
        Self {
            bits: TAG_POINTER,
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

    /// Check if value is callable (function or native function)
    #[inline]
    pub fn is_callable(&self) -> bool {
        self.is_function() || self.is_native_function()
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
    pub fn as_function(&self) -> Option<&Arc<Closure>> {
        match &self.heap_ref {
            Some(HeapRef::Function(f)) => Some(f),
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

    /// Get as promise
    pub fn as_promise(&self) -> Option<&Arc<JsPromise>> {
        match &self.heap_ref {
            Some(HeapRef::Promise(p)) => Some(p),
            _ => None,
        }
    }

    /// Get as proxy
    pub fn as_proxy(&self) -> Option<&Arc<JsProxy>> {
        match &self.heap_ref {
            Some(HeapRef::Proxy(p)) => Some(p),
            _ => None,
        }
    }

    /// Get as generator
    pub fn as_generator(&self) -> Option<&Arc<JsGenerator>> {
        match &self.heap_ref {
            Some(HeapRef::Generator(g)) => Some(g),
            _ => None,
        }
    }

    /// Get as regex
    pub fn as_regex(&self) -> Option<&Arc<JsRegExp>> {
        match &self.heap_ref {
            Some(HeapRef::RegExp(r)) => Some(r),
            _ => None,
        }
    }

    /// Get as ArrayBuffer
    pub fn as_array_buffer(&self) -> Option<&Arc<JsArrayBuffer>> {
        match &self.heap_ref {
            Some(HeapRef::ArrayBuffer(ab)) => Some(ab),
            _ => None,
        }
    }

    /// Get as TypedArray
    pub fn as_typed_array(&self) -> Option<&Arc<JsTypedArray>> {
        match &self.heap_ref {
            Some(HeapRef::TypedArray(ta)) => Some(ta),
            _ => None,
        }
    }

    /// Get as DataView
    pub fn as_data_view(&self) -> Option<&Arc<JsDataView>> {
        match &self.heap_ref {
            Some(HeapRef::DataView(dv)) => Some(dv),
            _ => None,
        }
    }

    /// Get as SharedArrayBuffer
    pub fn as_shared_array_buffer(&self) -> Option<&Arc<SharedArrayBuffer>> {
        match &self.heap_ref {
            Some(HeapRef::SharedArrayBuffer(sab)) => Some(sab),
            _ => None,
        }
    }

    /// Get as symbol
    pub fn as_symbol(&self) -> Option<&Arc<Symbol>> {
        match &self.heap_ref {
            Some(HeapRef::Symbol(s)) => Some(s),
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
            // These types use Arc, not GcRef, so they don't have GcHeaders
            Some(HeapRef::Symbol(_))
            | Some(HeapRef::BigInt(_))
            | Some(HeapRef::Promise(_))
            | Some(HeapRef::Proxy(_))
            | Some(HeapRef::ArrayBuffer(_))
            | Some(HeapRef::TypedArray(_))
            | Some(HeapRef::DataView(_))
            | Some(HeapRef::SharedArrayBuffer(_)) => None,
            None => None,
        }
    }

    /// Convert to boolean (ToBoolean)
    pub fn to_boolean(&self) -> bool {
        match self.bits {
            TAG_UNDEFINED | TAG_NULL | TAG_FALSE | TAG_NAN => false, // NaN is falsy
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
            TAG_UNDEFINED => "undefined",
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
                    | HeapRef::Proxy(_)
                    | HeapRef::Generator(_)
                    | HeapRef::ArrayBuffer(_)
                    | HeapRef::TypedArray(_)
                    | HeapRef::DataView(_)
                    | HeapRef::SharedArrayBuffer(_),
                ) => "object",
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
    fn test_value_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Value>();
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl Value {
    /// Trace GC references in this value
    pub(crate) fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        if let Some(heap_ref) = &self.heap_ref {
            match heap_ref {
                HeapRef::String(s) => {
                    tracer(s.header() as *const _);
                }
                HeapRef::Object(o) | HeapRef::Array(o) => {
                    tracer(o.header() as *const _);
                }
                HeapRef::Function(f) => {
                    tracer(f.object.header() as *const _);
                }
                HeapRef::NativeFunction(n) => {
                    tracer(n.object.header() as *const _);
                }
                HeapRef::RegExp(r) => {
                    tracer(r.object.header() as *const _);
                }
                HeapRef::Generator(g) => {
                    tracer(g.object.header() as *const _);
                }
                // Other types use Arc, not GcRef, so no GC tracing needed
                _ => {}
            }
        }
    }
}
