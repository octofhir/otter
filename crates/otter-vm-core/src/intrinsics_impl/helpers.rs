//! Helper functions for intrinsics (strict equality, SameValueZero, etc.)

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics::well_known;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use otter_macros::dive;

/// Strict equality (===) for Value, used by Array.prototype.indexOf etc.
pub fn strict_equal(a: &Value, b: &Value) -> bool {
    if let (Some(n1), Some(n2)) = (a.as_number(), b.as_number()) {
        n1 == n2 // NaN !== NaN, +0 === -0
    } else if (a.is_undefined() && b.is_undefined()) || (a.is_null() && b.is_null()) {
        true
    } else if let (Some(b1), Some(b2)) = (a.as_boolean(), b.as_boolean()) {
        b1 == b2
    } else if let (Some(s1), Some(s2)) = (a.as_string(), b.as_string()) {
        s1.as_str() == s2.as_str()
    } else if let (Some(sym1), Some(sym2)) = (a.as_symbol(), b.as_symbol()) {
        sym1.id == sym2.id
    } else if let (Some(o1), Some(o2)) = (a.as_object(), b.as_object()) {
        o1.as_ptr() == o2.as_ptr()
    } else if let (
        Some(crate::value::HeapRef::BigInt(ba)),
        Some(crate::value::HeapRef::BigInt(bb)),
    ) = (a.heap_ref(), b.heap_ref())
    {
        ba.value == bb.value
    } else {
        false
    }
}

/// SameValue comparison (ES2026 §6.1.6.1.14).
/// Like strict equality but NaN === NaN and +0 !== -0.
pub fn same_value(a: &Value, b: &Value) -> bool {
    if let (Some(n1), Some(n2)) = (a.as_number(), b.as_number()) {
        if n1.is_nan() && n2.is_nan() {
            return true;
        }
        if n1 == 0.0 && n2 == 0.0 {
            return n1.is_sign_positive() == n2.is_sign_positive();
        }
        n1 == n2
    } else {
        strict_equal(a, b)
    }
}

/// SameValueZero comparison (used by Array.prototype.includes, Set, Map).
/// Like strict equality but NaN === NaN.
pub fn same_value_zero(a: &Value, b: &Value) -> bool {
    if let (Some(n1), Some(n2)) = (a.as_number(), b.as_number()) {
        if n1.is_nan() && n2.is_nan() {
            return true;
        }
        n1 == n2
    } else {
        strict_equal(a, b)
    }
}

// ============================================================================
// MapKey: Value wrapper with SameValueZero Hash/Eq for Map/Set keys
// ============================================================================

/// A wrapper around `Value` that implements `Hash` and `Eq` using SameValueZero
/// semantics as required by ES2023 Map and Set.
///
/// SameValueZero: NaN equals NaN, -0 equals +0, otherwise strict equality.
#[derive(Clone)]
pub struct MapKey(pub Value);

impl MapKey {
    /// Returns a reference to the underlying `Value`.
    pub fn value(&self) -> &Value {
        &self.0
    }

    /// Consumes the `MapKey` and returns the underlying `Value`.
    pub fn into_value(self) -> Value {
        self.0
    }
}

// Type discriminant tags for hashing
const HASH_TAG_UNDEFINED: u8 = 0;
const HASH_TAG_NULL: u8 = 1;
const HASH_TAG_BOOL: u8 = 2;
const HASH_TAG_FLOAT64: u8 = 4;
const HASH_TAG_STRING: u8 = 5;
const HASH_TAG_SYMBOL: u8 = 6;
const HASH_TAG_OBJECT: u8 = 7;
const HASH_TAG_FUNCTION: u8 = 8;
const HASH_TAG_BIGINT: u8 = 9;
const HASH_TAG_NATIVE_FN: u8 = 10;
const HASH_TAG_PROXY: u8 = 11;
const HASH_TAG_PROMISE: u8 = 12;
const HASH_TAG_ARRAY: u8 = 13;

/// Normalize a float for SameValueZero hashing: -0 → +0, NaN → canonical NaN bits.
fn normalize_float_bits(n: f64) -> u64 {
    if n == 0.0 {
        0u64 // both +0 and -0 hash the same
    } else if n.is_nan() {
        0x7FF8_0000_0000_0000u64 // canonical NaN
    } else {
        n.to_bits()
    }
}

impl Hash for MapKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let v = &self.0;
        if v.is_undefined() {
            HASH_TAG_UNDEFINED.hash(state);
        } else if v.is_null() {
            HASH_TAG_NULL.hash(state);
        } else if let Some(b) = v.as_boolean() {
            HASH_TAG_BOOL.hash(state);
            b.hash(state);
        } else if let Some(i) = v.as_int32() {
            // Int32 and Float64 with same numeric value must hash the same
            HASH_TAG_FLOAT64.hash(state);
            normalize_float_bits(i as f64).hash(state);
        } else if let Some(n) = v.as_number() {
            HASH_TAG_FLOAT64.hash(state);
            normalize_float_bits(n).hash(state);
        } else if let Some(s) = v.as_string() {
            HASH_TAG_STRING.hash(state);
            s.as_str().hash(state);
        } else if let Some(sym) = v.as_symbol() {
            HASH_TAG_SYMBOL.hash(state);
            sym.id.hash(state);
        } else {
            // For heap-allocated reference types, use the HeapRef discriminant + pointer
            match v.heap_ref() {
                Some(crate::value::HeapRef::Object(obj)) => {
                    HASH_TAG_OBJECT.hash(state);
                    (obj.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::Array(arr)) => {
                    HASH_TAG_ARRAY.hash(state);
                    (arr.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::Function(f)) => {
                    HASH_TAG_FUNCTION.hash(state);
                    (f.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::NativeFunction(nf)) => {
                    HASH_TAG_NATIVE_FN.hash(state);
                    (nf.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::Proxy(p)) => {
                    HASH_TAG_PROXY.hash(state);
                    (p.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::Promise(p)) => {
                    HASH_TAG_PROMISE.hash(state);
                    (p.as_ptr() as usize).hash(state);
                }
                Some(crate::value::HeapRef::BigInt(b)) => {
                    HASH_TAG_BIGINT.hash(state);
                    b.value.hash(state);
                }
                _ => {
                    // Generator, ArrayBuffer, TypedArray, DataView, etc.
                    // Hash by type tag + a constant (identity not meaningful for maps)
                    255u8.hash(state);
                }
            }
        }
    }
}

impl PartialEq for MapKey {
    fn eq(&self, other: &Self) -> bool {
        same_value_zero(&self.0, &other.0)
    }
}

impl Eq for MapKey {}

impl std::fmt::Debug for MapKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MapKey({:?})", self.0)
    }
}

/// Get array length from object
pub fn get_array_length(obj: &crate::gc::GcRef<crate::object::JsObject>) -> usize {
    obj.get(&crate::object::PropertyKey::string("length"))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as usize
}

/// Set array length on object
pub fn set_array_length(obj: &crate::gc::GcRef<crate::object::JsObject>, len: usize) {
    let _ = obj.set(
        crate::object::PropertyKey::string("length"),
        Value::number(len as f64),
    );
}

#[dive(name = "[Symbol.species]", length = 0)]
fn species_getter(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    Ok(this_val.clone())
}

/// Define the standard @@species getter on a constructor.
///
/// Per ES spec, this is an accessor on the constructor that returns `this`
/// and has name "get [Symbol.species]".
pub fn define_species_getter(
    ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let (_species_name, species_native, _species_len) = species_getter_decl();
    let getter_object = GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));
    getter_object.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(
            "get [Symbol.species]",
        ))),
    );
    getter_object.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::int32(0)),
    );
    let getter = Value::native_function_with_proto_and_object(
        species_native,
        mm.clone(),
        fn_proto,
        getter_object,
    );
    ctor.define_property(
        PropertyKey::Symbol(well_known::species_symbol()),
        PropertyDescriptor::Accessor {
            get: Some(getter),
            set: None,
            attributes: PropertyAttributes::builtin_accessor(),
        },
    );
}
