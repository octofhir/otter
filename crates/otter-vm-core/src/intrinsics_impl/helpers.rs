//! Helper functions for intrinsics (strict equality, SameValueZero, etc.)

use crate::value::Value;

/// Strict equality (===) for Value, used by Array.prototype.indexOf etc.
pub fn strict_equal(a: &Value, b: &Value) -> bool {
    if let (Some(n1), Some(n2)) = (a.as_number(), b.as_number()) {
        n1 == n2 // NaN !== NaN, +0 === -0
    } else if a.is_undefined() && b.is_undefined() {
        true
    } else if a.is_null() && b.is_null() {
        true
    } else if let (Some(b1), Some(b2)) = (a.as_boolean(), b.as_boolean()) {
        b1 == b2
    } else if let (Some(s1), Some(s2)) = (a.as_string(), b.as_string()) {
        s1.as_str() == s2.as_str()
    } else if let (Some(sym1), Some(sym2)) = (a.as_symbol(), b.as_symbol()) {
        sym1.id == sym2.id
    } else if let (Some(o1), Some(o2)) = (a.as_object(), b.as_object()) {
        o1.as_ptr() == o2.as_ptr()
    } else {
        false
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

/// Get array length from object
pub fn get_array_length(obj: &crate::gc::GcRef<crate::object::JsObject>) -> usize {
    obj.get(&crate::object::PropertyKey::string("length"))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as usize
}

/// Set array length on object
pub fn set_array_length(obj: &crate::gc::GcRef<crate::object::JsObject>, len: usize) {
    obj.set(
        crate::object::PropertyKey::string("length"),
        Value::number(len as f64),
    );
}
