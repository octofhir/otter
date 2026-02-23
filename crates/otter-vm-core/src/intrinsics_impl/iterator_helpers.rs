//! Iterator Helpers (ES2025)
//!
//! Implements Iterator.prototype lazy methods (map, filter, take, drop, flatMap),
//! terminal methods (reduce, toArray, forEach, some, every, find), and
//! Iterator constructor + Iterator.from().
//!
//! ## Prototype Chain
//! ```text
//! Object.prototype
//!   └── %IteratorPrototype% (+ map, filter, take, drop, flatMap, reduce, toArray, ...)
//!         ├── %IteratorHelperPrototype% (next, return, [Symbol.toStringTag])
//!         └── %WrapForValidIteratorPrototype% (next, return)
//! ```

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

// ============================================================================
// Internal slot keys
// ============================================================================
const UNDERLYING_ITER: &str = "__underlying_iter__";
const UNDERLYING_NEXT: &str = "__underlying_next__";
const CALLBACK: &str = "__callback__";
const HELPER_KIND: &str = "__helper_kind__";
const REMAINING: &str = "__remaining__";
const COUNTER: &str = "__counter__";
const INNER_ITER: &str = "__inner_iter__";
const INNER_NEXT: &str = "__inner_next__";
const ITER_DONE: &str = "__iter_done__";
const ALIVE: &str = "__alive__";

// ============================================================================
// Helpers
// ============================================================================

fn pk(s: &str) -> PropertyKey {
    PropertyKey::String(JsString::intern(s))
}

/// Create a native function Value with proper `name`, `length`, `__non_constructor`.
fn make_builtin<F>(
    name: &str,
    length: i32,
    f: F,
    mm: Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) -> Value
where
    F: Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let val = Value::native_function_with_proto(f, mm, fn_proto);
    if let Some(obj) = val.native_function_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        let _ = obj.set(pk("__non_constructor"), Value::boolean(true));
    }
    val
}

/// Create an iterator result object { value, done }
fn create_iter_result(value: Value, done: bool, ncx: &mut NativeContext<'_>) -> Value {
    let proto = ncx
        .global()
        .get(&PropertyKey::string("Object"))
        .and_then(|o| o.as_object().or_else(|| o.native_function_object()))
        .and_then(|o| o.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .map(Value::object)
        .unwrap_or(Value::null());
    let result = GcRef::new(JsObject::new(proto, ncx.memory_manager().clone()));
    let _ = result.set(pk("value"), value);
    let _ = result.set(pk("done"), Value::boolean(done));
    Value::object(result)
}

/// Get the `next` method from an iterator object.
fn get_iterator_next(iter: &Value, ncx: &mut NativeContext<'_>) -> Result<Value, VmError> {
    if let Some(obj) = iter.as_object().or_else(|| iter.native_function_object()) {
        if let Some(next) = obj.get(&pk("next")) {
            return Ok(next);
        }
    }
    // Try property access via proxy/getter chain
    if let Some(obj) = iter.as_object() {
        let next = ncx.get_property(&obj, &pk("next"))?;
        return Ok(next);
    }
    Err(VmError::type_error(
        "Iterator.prototype method requires an iterator with a next method",
    ))
}

/// Call next() on the underlying iterator.
fn iter_step(
    iter: &Value,
    next_method: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<(Value, bool), VmError> {
    let result = ncx.call_function(next_method, iter.clone(), &[])?;
    let done = result
        .as_object()
        .or_else(|| result.native_function_object())
        .and_then(|o| o.get(&pk("done")))
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let value = result
        .as_object()
        .or_else(|| result.native_function_object())
        .and_then(|o| o.get(&pk("value")))
        .unwrap_or_else(Value::undefined);
    Ok((value, done))
}

/// Call the return() method on the underlying iterator if it exists.
fn iter_close(iter: &Value, ncx: &mut NativeContext<'_>) -> Result<Value, VmError> {
    if let Some(obj) = iter.as_object().or_else(|| iter.native_function_object()) {
        if let Some(return_fn) = obj.get(&pk("return")) {
            if return_fn.is_function() || return_fn.is_native_function() {
                let result = ncx.call_function(&return_fn, Value::object(obj), &[])?;
                return Ok(result);
            }
        }
    }
    Ok(Value::undefined())
}

/// Require `this` to be an object, extract underlying iter and next.
fn require_iterator_helper(
    this: &Value,
) -> Result<(GcRef<JsObject>, Value, Value, String), VmError> {
    let obj = this
        .as_object()
        .ok_or_else(|| VmError::type_error("Iterator helper method called on non-object"))?;
    let iter = obj
        .get(&pk(UNDERLYING_ITER))
        .ok_or_else(|| VmError::type_error("Not an iterator helper object"))?;
    let next = obj
        .get(&pk(UNDERLYING_NEXT))
        .ok_or_else(|| VmError::type_error("Not an iterator helper object"))?;
    let kind = obj
        .get(&pk(HELPER_KIND))
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_default();
    Ok((obj, iter, next, kind))
}

// ============================================================================
// Iterator Helper factory: creates IteratorHelper objects for lazy methods
// ============================================================================

fn make_iterator_helper(
    underlying: &Value,
    next_method: Value,
    kind: &str,
    callback: Option<Value>,
    remaining: Option<f64>,
    mm: Arc<MemoryManager>,
    iter_helper_proto: GcRef<JsObject>,
) -> Value {
    let helper = GcRef::new(JsObject::new(
        Value::object(iter_helper_proto),
        mm,
    ));
    let _ = helper.set(pk(UNDERLYING_ITER), underlying.clone());
    let _ = helper.set(pk(UNDERLYING_NEXT), next_method);
    let _ = helper.set(pk(HELPER_KIND), Value::string(JsString::intern(kind)));
    let _ = helper.set(pk(ITER_DONE), Value::boolean(false));
    let _ = helper.set(pk(ALIVE), Value::boolean(true));
    let _ = helper.set(pk(COUNTER), Value::number(0.0));
    if let Some(cb) = callback {
        let _ = helper.set(pk(CALLBACK), cb);
    }
    if let Some(rem) = remaining {
        let _ = helper.set(pk(REMAINING), Value::number(rem));
    }
    Value::object(helper)
}

// ============================================================================
// IteratorHelper.prototype.next() — dispatches on __helper_kind__
// ============================================================================

fn iterator_helper_next(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let (obj, iter, next, kind) = require_iterator_helper(this_val)?;

    // Check if already done
    if obj
        .get(&pk(ITER_DONE))
        .and_then(|v| v.as_boolean())
        .unwrap_or(false)
    {
        return Ok(create_iter_result(Value::undefined(), true, ncx));
    }

    match kind.as_str() {
        "map" => helper_next_map(&obj, &iter, &next, ncx),
        "filter" => helper_next_filter(&obj, &iter, &next, ncx),
        "take" => helper_next_take(&obj, &iter, &next, ncx),
        "drop" => helper_next_drop(&obj, &iter, &next, ncx),
        "flatMap" => helper_next_flat_map(&obj, &iter, &next, ncx),
        _ => Err(VmError::type_error("Unknown iterator helper kind")),
    }
}

fn helper_next_map(
    obj: &GcRef<JsObject>,
    iter: &Value,
    next: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = obj
        .get(&pk(CALLBACK))
        .ok_or_else(|| VmError::type_error("Iterator helper: missing callback"))?;
    let counter = obj
        .get(&pk(COUNTER))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0);

    let (value, done) = iter_step(iter, next, ncx)?;
    if done {
        let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
        return Ok(create_iter_result(Value::undefined(), true, ncx));
    }

    let _ = obj.set(pk(COUNTER), Value::number(counter + 1.0));
    let mapped = ncx.call_function(&callback, Value::undefined(), &[value, Value::number(counter)])?;
    Ok(create_iter_result(mapped, false, ncx))
}

fn helper_next_filter(
    obj: &GcRef<JsObject>,
    iter: &Value,
    next: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = obj
        .get(&pk(CALLBACK))
        .ok_or_else(|| VmError::type_error("Iterator helper: missing callback"))?;
    let mut counter = obj
        .get(&pk(COUNTER))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0);

    loop {
        let (value, done) = iter_step(iter, next, ncx)?;
        if done {
            let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
            return Ok(create_iter_result(Value::undefined(), true, ncx));
        }

        let idx = counter;
        counter += 1.0;
        let _ = obj.set(pk(COUNTER), Value::number(counter));
        let selected =
            ncx.call_function(&callback, Value::undefined(), &[value.clone(), Value::number(idx)])?;
        if selected.to_boolean() {
            return Ok(create_iter_result(value, false, ncx));
        }
    }
}

fn helper_next_take(
    obj: &GcRef<JsObject>,
    iter: &Value,
    next: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let remaining = obj
        .get(&pk(REMAINING))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0);

    if remaining <= 0.0 {
        let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
        let _ = iter_close(iter, ncx);
        return Ok(create_iter_result(Value::undefined(), true, ncx));
    }

    let _ = obj.set(pk(REMAINING), Value::number(remaining - 1.0));

    let (value, done) = iter_step(iter, next, ncx)?;
    if done {
        let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
        return Ok(create_iter_result(Value::undefined(), true, ncx));
    }

    Ok(create_iter_result(value, false, ncx))
}

fn helper_next_drop(
    obj: &GcRef<JsObject>,
    iter: &Value,
    next: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let mut remaining = obj
        .get(&pk(REMAINING))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0);

    // Drop the first N values
    while remaining > 0.0 {
        let (_, done) = iter_step(iter, next, ncx)?;
        if done {
            let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
            let _ = obj.set(pk(REMAINING), Value::number(0.0));
            return Ok(create_iter_result(Value::undefined(), true, ncx));
        }
        remaining -= 1.0;
    }
    let _ = obj.set(pk(REMAINING), Value::number(0.0));

    // After dropping, yield from underlying
    let (value, done) = iter_step(iter, next, ncx)?;
    if done {
        let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
        return Ok(create_iter_result(Value::undefined(), true, ncx));
    }

    Ok(create_iter_result(value, false, ncx))
}

fn helper_next_flat_map(
    obj: &GcRef<JsObject>,
    iter: &Value,
    next: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = obj
        .get(&pk(CALLBACK))
        .ok_or_else(|| VmError::type_error("Iterator helper: missing callback"))?;

    loop {
        // If we have an active inner iterator, try to get from it
        if let Some(inner_iter) = obj.get(&pk(INNER_ITER)) {
            if !inner_iter.is_undefined() && !inner_iter.is_null() {
                if let Some(inner_next) = obj.get(&pk(INNER_NEXT)) {
                    let (value, done) = iter_step(&inner_iter, &inner_next, ncx)?;
                    if !done {
                        return Ok(create_iter_result(value, false, ncx));
                    }
                    // Inner iterator exhausted, clear it
                    let _ = obj.set(pk(INNER_ITER), Value::undefined());
                    let _ = obj.set(pk(INNER_NEXT), Value::undefined());
                }
            }
        }

        // Get next from outer iterator
        let counter = obj
            .get(&pk(COUNTER))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);

        let (value, done) = iter_step(iter, next, ncx)?;
        if done {
            let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
            return Ok(create_iter_result(Value::undefined(), true, ncx));
        }

        let _ = obj.set(pk(COUNTER), Value::number(counter + 1.0));

        // Call mapper — result should be iterable
        let mapped =
            ncx.call_function(&callback, Value::undefined(), &[value, Value::number(counter)])?;

        // Get iterator from mapped value
        let inner_iter_val = get_iterator(&mapped, ncx)?;
        let inner_next_val = get_iterator_next(&inner_iter_val, ncx)?;

        // Store inner iterator
        let _ = obj.set(pk(INNER_ITER), inner_iter_val.clone());
        let _ = obj.set(pk(INNER_NEXT), inner_next_val.clone());

        // Try to get the first value from the inner iterator
        let (inner_value, inner_done) = iter_step(&inner_iter_val, &inner_next_val, ncx)?;
        if !inner_done {
            return Ok(create_iter_result(inner_value, false, ncx));
        }
        // Inner iterator was empty, continue with next outer value
        let _ = obj.set(pk(INNER_ITER), Value::undefined());
        let _ = obj.set(pk(INNER_NEXT), Value::undefined());
    }
}

// ============================================================================
// IteratorHelper.prototype.return() — close underlying + inner iterator
// ============================================================================

fn iterator_helper_return(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("Iterator helper method called on non-object"))?;

    let _ = obj.set(pk(ITER_DONE), Value::boolean(true));
    let _ = obj.set(pk(ALIVE), Value::boolean(false));

    // Close inner iterator if present (flatMap)
    if let Some(inner) = obj.get(&pk(INNER_ITER)) {
        if !inner.is_undefined() && !inner.is_null() {
            let _ = iter_close(&inner, ncx);
            let _ = obj.set(pk(INNER_ITER), Value::undefined());
        }
    }

    // Close underlying iterator
    if let Some(iter) = obj.get(&pk(UNDERLYING_ITER)) {
        let _ = iter_close(&iter, ncx);
    }

    Ok(create_iter_result(Value::undefined(), true, ncx))
}

// ============================================================================
// Get iterator from an object (call [Symbol.iterator]())
// ============================================================================

fn get_iterator(value: &Value, ncx: &mut NativeContext<'_>) -> Result<Value, VmError> {
    // If it's already an iterator (has next method), return as-is
    // Try Symbol.iterator first
    let sym_iter = crate::intrinsics::well_known::iterator_symbol();
    if let Some(obj) = value.as_object().or_else(|| value.native_function_object()) {
        if let Some(method) = obj.get(&PropertyKey::Symbol(sym_iter)) {
            if method.is_function() || method.is_native_function() {
                let iter_val = ncx.call_function(&method, value.clone(), &[])?;
                return Ok(iter_val);
            }
        }
    }
    Err(VmError::type_error("value is not iterable"))
}

// ============================================================================
// GetIteratorDirect (ES2025 §7.4.1) — used by Iterator.prototype methods
// ============================================================================

/// Get the `next` method from `this` for Iterator.prototype methods.
/// ES2025: Let `nextMethod` be ? Get(obj, "next"). Return { [[Iterator]]: obj, [[NextMethod]]: nextMethod }.
fn get_iterator_direct(
    this: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<(Value, Value), VmError> {
    let obj = this
        .as_object()
        .or_else(|| this.native_function_object())
        .ok_or_else(|| {
            VmError::type_error("Iterator.prototype method called on non-object")
        })?;
    let next = ncx.get_property(&obj, &pk("next"))?;
    Ok((this.clone(), next))
}

// ============================================================================
// Terminal methods on Iterator.prototype
// ============================================================================

fn iterator_to_array(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let arr = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
    let mut idx = 0u32;
    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            break;
        }
        let _ = arr.set(PropertyKey::Index(idx), value);
        idx += 1;
    }
    let _ = arr.set(pk("length"), Value::number(idx as f64));
    Ok(Value::array(arr))
}

fn iterator_for_each(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.forEach callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let mut counter = 0.0f64;
    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            break;
        }
        ncx.call_function(&callback, Value::undefined(), &[value, Value::number(counter)])?;
        counter += 1.0;
    }
    Ok(Value::undefined())
}

fn iterator_reduce(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.reduce callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let mut counter = 0.0f64;

    let mut accumulator = if args.len() >= 2 {
        args[1].clone()
    } else {
        // Use first element as initial value
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            return Err(VmError::type_error(
                "Reduce of empty iterator with no initial value",
            ));
        }
        counter = 1.0;
        value
    };

    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            break;
        }
        accumulator = ncx.call_function(
            &callback,
            Value::undefined(),
            &[accumulator, value, Value::number(counter)],
        )?;
        counter += 1.0;
    }
    Ok(accumulator)
}

fn iterator_some(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.some callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let mut counter = 0.0f64;
    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            return Ok(Value::boolean(false));
        }
        let result =
            ncx.call_function(&callback, Value::undefined(), &[value, Value::number(counter)])?;
        counter += 1.0;
        if result.to_boolean() {
            let _ = iter_close(&iter, ncx);
            return Ok(Value::boolean(true));
        }
    }
}

fn iterator_every(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.every callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let mut counter = 0.0f64;
    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            return Ok(Value::boolean(true));
        }
        let result =
            ncx.call_function(&callback, Value::undefined(), &[value, Value::number(counter)])?;
        counter += 1.0;
        if !result.to_boolean() {
            let _ = iter_close(&iter, ncx);
            return Ok(Value::boolean(false));
        }
    }
}

fn iterator_find(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.find callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    let mut counter = 0.0f64;
    loop {
        let (value, done) = iter_step(&iter, &next, ncx)?;
        if done {
            return Ok(Value::undefined());
        }
        let result = ncx.call_function(
            &callback,
            Value::undefined(),
            &[value.clone(), Value::number(counter)],
        )?;
        counter += 1.0;
        if result.to_boolean() {
            let _ = iter_close(&iter, ncx);
            return Ok(value);
        }
    }
}

// ============================================================================
// Lazy method factories (installed on Iterator.prototype)
// ============================================================================

fn iterator_map(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    iter_helper_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.map callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    Ok(make_iterator_helper(
        &iter,
        next,
        "map",
        Some(callback),
        None,
        ncx.memory_manager().clone(),
        iter_helper_proto,
    ))
}

fn iterator_filter(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    iter_helper_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.filter callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    Ok(make_iterator_helper(
        &iter,
        next,
        "filter",
        Some(callback),
        None,
        ncx.memory_manager().clone(),
        iter_helper_proto,
    ))
}

fn iterator_take(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    iter_helper_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let limit = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(f64::NAN);
    if limit.is_nan() || limit < 0.0 {
        return Err(VmError::range_error(
            "Iterator.prototype.take requires a non-negative number",
        ));
    }
    let limit = limit.trunc();
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    Ok(make_iterator_helper(
        &iter,
        next,
        "take",
        None,
        Some(limit),
        ncx.memory_manager().clone(),
        iter_helper_proto,
    ))
}

fn iterator_drop(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    iter_helper_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let limit = args
        .first()
        .and_then(|v| v.as_number())
        .unwrap_or(f64::NAN);
    if limit.is_nan() || limit < 0.0 {
        return Err(VmError::range_error(
            "Iterator.prototype.drop requires a non-negative number",
        ));
    }
    let limit = limit.trunc();
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    Ok(make_iterator_helper(
        &iter,
        next,
        "drop",
        None,
        Some(limit),
        ncx.memory_manager().clone(),
        iter_helper_proto,
    ))
}

fn iterator_flat_map(
    this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    iter_helper_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let callback = args
        .first()
        .cloned()
        .unwrap_or_else(Value::undefined);
    if !callback.is_function() && !callback.is_native_function() {
        return Err(VmError::type_error(
            "Iterator.prototype.flatMap callback is not a function",
        ));
    }
    let (iter, next) = get_iterator_direct(this_val, ncx)?;
    Ok(make_iterator_helper(
        &iter,
        next,
        "flatMap",
        Some(callback),
        None,
        ncx.memory_manager().clone(),
        iter_helper_proto,
    ))
}

// ============================================================================
// Iterator constructor (abstract) + Iterator.from()
// ============================================================================

fn iterator_constructor(
    _this: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    Err(VmError::type_error(
        "Iterator is not directly constructable",
    ))
}

fn iterator_from(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
    wrap_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let value = args.first().cloned().unwrap_or_else(Value::undefined);

    // If value has Symbol.iterator, get an iterator from it
    let sym_iter = crate::intrinsics::well_known::iterator_symbol();
    if let Some(obj) = value.as_object().or_else(|| value.native_function_object()) {
        if let Some(method) = obj.get(&PropertyKey::Symbol(sym_iter)) {
            if method.is_function() || method.is_native_function() {
                let iter_result = ncx.call_function(&method, value.clone(), &[])?;
                // Check if the result already has Iterator.prototype in its chain
                // For simplicity, if it has a `next` method, wrap it
                return Ok(wrap_for_valid_iterator(
                    &iter_result,
                    ncx,
                    wrap_proto,
                )?);
            }
        }
    }

    // Object with a `next` method — wrap it
    if let Some(obj) = value.as_object().or_else(|| value.native_function_object()) {
        if obj.get(&pk("next")).is_some() {
            return Ok(wrap_for_valid_iterator(&value, ncx, wrap_proto)?);
        }
    }

    Err(VmError::type_error(
        "Iterator.from requires an iterable or iterator-like object",
    ))
}

/// Create a WrapForValidIterator that wraps an arbitrary iterator-like object.
fn wrap_for_valid_iterator(
    iter: &Value,
    ncx: &mut NativeContext<'_>,
    wrap_proto: GcRef<JsObject>,
) -> Result<Value, VmError> {
    let next_method = get_iterator_next(iter, ncx)?;
    let wrapper = GcRef::new(JsObject::new(
        Value::object(wrap_proto),
        ncx.memory_manager().clone(),
    ));
    let _ = wrapper.set(pk(UNDERLYING_ITER), iter.clone());
    let _ = wrapper.set(pk(UNDERLYING_NEXT), next_method);
    Ok(Value::object(wrapper))
}

// ============================================================================
// WrapForValidIterator.prototype.next / return
// ============================================================================

fn wrap_next(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("WrapForValidIterator: not an object"))?;
    let iter = obj
        .get(&pk(UNDERLYING_ITER))
        .ok_or_else(|| VmError::type_error("Not a WrapForValidIterator"))?;
    let next = obj
        .get(&pk(UNDERLYING_NEXT))
        .ok_or_else(|| VmError::type_error("Not a WrapForValidIterator"))?;
    let result = ncx.call_function(&next, iter, &[])?;
    // Ensure the result is an object
    if result.as_object().is_none() && result.native_function_object().is_none() {
        return Err(VmError::type_error(
            "Iterator result must be an object",
        ));
    }
    Ok(result)
}

fn wrap_return(
    this_val: &Value,
    _args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("WrapForValidIterator: not an object"))?;
    if let Some(iter) = obj.get(&pk(UNDERLYING_ITER)) {
        let _ = iter_close(&iter, ncx);
    }
    Ok(create_iter_result(Value::undefined(), true, ncx))
}

// ============================================================================
// Public initialization functions
// ============================================================================

/// Initialize Iterator.prototype methods: lazy helpers + terminal methods.
/// Also sets up %IteratorHelperPrototype% and %WrapForValidIteratorPrototype%.
///
/// Called from `Intrinsics::initialize_prototypes()`.
pub fn init_iterator_prototype(
    iterator_prototype: GcRef<JsObject>,
    iterator_helper_prototype: GcRef<JsObject>,
    wrap_for_valid_iterator_prototype: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_iterator: GcRef<crate::value::Symbol>,
    symbol_to_string_tag: GcRef<crate::value::Symbol>,
) {
    // ---- %IteratorHelperPrototype% setup ----
    // Chain: IteratorHelperPrototype -> IteratorPrototype
    iterator_helper_prototype.set_prototype(Value::object(iterator_prototype));

    // next()
    iterator_helper_prototype.define_property(
        pk("next"),
        PropertyDescriptor::builtin_method(make_builtin(
            "next",
            0,
            iterator_helper_next,
            mm.clone(),
            fn_proto,
        )),
    );

    // return()
    iterator_helper_prototype.define_property(
        pk("return"),
        PropertyDescriptor::builtin_method(make_builtin(
            "return",
            0,
            iterator_helper_return,
            mm.clone(),
            fn_proto,
        )),
    );

    // [Symbol.toStringTag] = "Iterator Helper"
    iterator_helper_prototype.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::Data {
            value: Value::string(JsString::intern("Iterator Helper")),
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // ---- %WrapForValidIteratorPrototype% setup ----
    // Chain: WrapForValidIteratorPrototype -> IteratorPrototype
    wrap_for_valid_iterator_prototype.set_prototype(Value::object(iterator_prototype));

    wrap_for_valid_iterator_prototype.define_property(
        pk("next"),
        PropertyDescriptor::builtin_method(make_builtin(
            "next",
            0,
            wrap_next,
            mm.clone(),
            fn_proto,
        )),
    );

    wrap_for_valid_iterator_prototype.define_property(
        pk("return"),
        PropertyDescriptor::builtin_method(make_builtin(
            "return",
            0,
            wrap_return,
            mm.clone(),
            fn_proto,
        )),
    );

    // ---- Lazy methods on Iterator.prototype (return IteratorHelper objects) ----
    {
        let ihp = iterator_helper_prototype;
        let mm = mm.clone();

        // .map(callback)
        let ihp_map = ihp;
        let mm_map = mm.clone();
        iterator_prototype.define_property(
            pk("map"),
            PropertyDescriptor::builtin_method(make_builtin(
                "map",
                1,
                move |this, args, ncx| iterator_map(this, args, ncx, ihp_map),
                mm_map,
                fn_proto,
            )),
        );

        // .filter(callback)
        let ihp_filter = ihp;
        let mm_filter = mm.clone();
        iterator_prototype.define_property(
            pk("filter"),
            PropertyDescriptor::builtin_method(make_builtin(
                "filter",
                1,
                move |this, args, ncx| iterator_filter(this, args, ncx, ihp_filter),
                mm_filter,
                fn_proto,
            )),
        );

        // .take(limit)
        let ihp_take = ihp;
        let mm_take = mm.clone();
        iterator_prototype.define_property(
            pk("take"),
            PropertyDescriptor::builtin_method(make_builtin(
                "take",
                1,
                move |this, args, ncx| iterator_take(this, args, ncx, ihp_take),
                mm_take,
                fn_proto,
            )),
        );

        // .drop(limit)
        let ihp_drop = ihp;
        let mm_drop = mm.clone();
        iterator_prototype.define_property(
            pk("drop"),
            PropertyDescriptor::builtin_method(make_builtin(
                "drop",
                1,
                move |this, args, ncx| iterator_drop(this, args, ncx, ihp_drop),
                mm_drop,
                fn_proto,
            )),
        );

        // .flatMap(callback)
        let ihp_flat = ihp;
        let mm_flat = mm.clone();
        iterator_prototype.define_property(
            pk("flatMap"),
            PropertyDescriptor::builtin_method(make_builtin(
                "flatMap",
                1,
                move |this, args, ncx| iterator_flat_map(this, args, ncx, ihp_flat),
                mm_flat,
                fn_proto,
            )),
        );
    }

    // ---- Terminal methods on Iterator.prototype ----
    iterator_prototype.define_property(
        pk("toArray"),
        PropertyDescriptor::builtin_method(make_builtin(
            "toArray",
            0,
            iterator_to_array,
            mm.clone(),
            fn_proto,
        )),
    );

    iterator_prototype.define_property(
        pk("forEach"),
        PropertyDescriptor::builtin_method(make_builtin(
            "forEach",
            1,
            iterator_for_each,
            mm.clone(),
            fn_proto,
        )),
    );

    iterator_prototype.define_property(
        pk("reduce"),
        PropertyDescriptor::builtin_method(make_builtin(
            "reduce",
            1,
            iterator_reduce,
            mm.clone(),
            fn_proto,
        )),
    );

    iterator_prototype.define_property(
        pk("some"),
        PropertyDescriptor::builtin_method(make_builtin(
            "some",
            1,
            iterator_some,
            mm.clone(),
            fn_proto,
        )),
    );

    iterator_prototype.define_property(
        pk("every"),
        PropertyDescriptor::builtin_method(make_builtin(
            "every",
            1,
            iterator_every,
            mm.clone(),
            fn_proto,
        )),
    );

    iterator_prototype.define_property(
        pk("find"),
        PropertyDescriptor::builtin_method(make_builtin(
            "find",
            1,
            iterator_find,
            mm.clone(),
            fn_proto,
        )),
    );

    // ---- [Symbol.iterator]() { return this; } is already set by intrinsics.rs ----
    // We don't override it here.

    // ---- Iterator.prototype.constructor = Iterator ----
    // This is set when the Iterator constructor is installed on the global object.
    let _ = symbol_iterator; // Used by caller for wiring
}

/// Install the global `Iterator` constructor with `Iterator.from()` static method.
pub fn install_iterator_constructor(
    global: GcRef<JsObject>,
    iterator_prototype: GcRef<JsObject>,
    wrap_for_valid_iterator_prototype: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Create Iterator constructor function
    let ctor = Value::native_function_with_proto(
        iterator_constructor,
        mm.clone(),
        fn_proto,
    );
    if let Some(ctor_obj) = ctor.native_function_object() {
        ctor_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(0)),
        );
        ctor_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("Iterator"))),
        );

        // Iterator.prototype
        ctor_obj.define_property(
            PropertyKey::string("prototype"),
            PropertyDescriptor::Data {
                value: Value::object(iterator_prototype),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            },
        );

        // Iterator.from()
        let wfvip = wrap_for_valid_iterator_prototype;
        let mm_from = mm.clone();
        ctor_obj.define_property(
            PropertyKey::string("from"),
            PropertyDescriptor::builtin_method(make_builtin(
                "from",
                1,
                move |this, args, ncx| iterator_from(this, args, ncx, wfvip),
                mm_from,
                fn_proto,
            )),
        );
    }

    // Iterator.prototype.constructor = Iterator
    iterator_prototype.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::Data {
            value: ctor.clone(),
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );

    // Install on global object
    global.define_property(
        PropertyKey::string("Iterator"),
        PropertyDescriptor::Data {
            value: ctor,
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: true,
            },
        },
    );
}
