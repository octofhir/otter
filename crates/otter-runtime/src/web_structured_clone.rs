//! In-realm `structuredClone` — HTML StructuredSerialize/StructuredDeserialize
//! performed directly on VM values (no cross-thread serialization).
//!
//! # Contents
//! - [`structured_clone`] — clone a value's reachable platform object graph,
//!   preserving internal references and cycles, throwing `DataCloneError` for
//!   non-serializable inputs.
//!
//! # Invariants
//! - Clones are kept reachable across the recursion through the interpreter's
//!   module-root stack (GC-traced + relocated in place); the recursion returns
//!   a stack index rather than a bare [`Value`] so no handle is held across an
//!   allocation. The memory map is keyed by the source object's raw handle so
//!   shared references and cycles are reconstructed exactly once.
//! - Complex platform objects (Date / RegExp / typed arrays / DataView /
//!   ArrayBuffer / Error) are rebuilt through their real constructors via
//!   [`NativeCtx::construct`] so prototypes and brand checks are correct.
//!
//! # See also
//! - <https://html.spec.whatwg.org/multipage/structured-data.html>

use std::collections::HashMap;

use otter_gc::raw::RawGc;
use otter_vm::binary::TypedArrayKind;
use otter_vm::number::NumberValue;
use otter_vm::{NativeCtx, NativeError, Value, array, collections, object};

/// `structuredClone(value)` — clone `value` within the current realm.
///
/// # Errors
/// Throws `DataCloneError` (as a native thrown error) when the graph contains a
/// function, symbol, or other non-serializable platform object.
pub fn structured_clone(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    let base = ctx.interp_mut().module_root_depth();
    let mut memo: HashMap<RawGc, usize> = HashMap::new();
    let result = clone_to_index(ctx, value, &mut memo).map(|idx| ctx.interp_mut().module_root(idx));
    ctx.interp_mut().pop_module_roots_to(base);
    result
}

fn push_root(ctx: &mut NativeCtx<'_>, value: Value) -> usize {
    ctx.interp_mut().push_module_root(value) - 1
}

fn read_root(ctx: &mut NativeCtx<'_>, idx: usize) -> Value {
    ctx.interp_mut().module_root(idx)
}

fn data_clone_error(kind: &str) -> NativeError {
    NativeError::TypeError {
        name: "structuredClone",
        reason: format!("{kind} could not be cloned (DataCloneError)"),
    }
}

fn type_error(message: String) -> NativeError {
    NativeError::TypeError {
        name: "structuredClone",
        reason: message,
    }
}

/// Resolve a global constructor (`Date`, `RegExp`, `Uint8Array`, …).
fn ctor(ctx: &mut NativeCtx<'_>, name: &str) -> Result<Value, NativeError> {
    ctx.global_value(name)
        .filter(|v| !v.is_undefined() && !v.is_null())
        .ok_or_else(|| type_error(format!("global {name} is unavailable")))
}

fn number_value(n: f64) -> Value {
    Value::number(NumberValue::from_f64(n))
}

/// Clone `value`, returning the module-root-stack index of the clone.
fn clone_to_index(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    memo: &mut HashMap<RawGc, usize>,
) -> Result<usize, NativeError> {
    // Primitives are immutable — clone is identity.
    if value.is_undefined()
        || value.is_null()
        || value.as_boolean().is_some()
        || value.as_number().is_some()
        || value.as_big_int().is_some()
        || value.as_string(ctx.heap()).is_some()
    {
        return Ok(push_root(ctx, value));
    }
    if value.is_symbol() {
        return Err(data_clone_error("Symbol"));
    }
    if value.is_function() {
        return Err(data_clone_error("Function"));
    }

    let raw = value.as_raw_gc().ok_or_else(|| data_clone_error("value"))?;

    // ArrayBuffer.
    if let Some(buf) = value.as_array_buffer() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let bytes = buf.with_bytes(ctx.heap(), |b| b.to_vec());
        let cloned = ctx
            .array_buffer_from_bytes_rooted(bytes, &[], &[])
            .map_err(|e| type_error(e.to_string()))?;
        let idx = push_root(ctx, Value::array_buffer(cloned));
        memo.insert(raw, idx);
        return Ok(idx);
    }

    // Typed arrays.
    if let Some(ta) = value.as_typed_array(ctx.heap()) {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let kind = ta.kind();
        let byte_offset = ta.byte_offset(ctx.heap());
        let length = ta.length(ctx.heap());
        let bytes = ta.buffer(ctx.heap()).with_bytes(ctx.heap(), |b| b.to_vec());
        let buffer = ctx
            .array_buffer_from_bytes_rooted(bytes, &[], &[])
            .map_err(|e| type_error(e.to_string()))?;
        let buffer_idx = push_root(ctx, Value::array_buffer(buffer));
        let ctor_val = ctor(ctx, typed_array_ctor_name(kind))?;
        let buffer_val = read_root(ctx, buffer_idx);
        let instance = ctx.construct(
            ctor_val,
            &[
                buffer_val,
                number_value(byte_offset as f64),
                number_value(length as f64),
            ],
        )?;
        let idx = push_root(ctx, instance);
        memo.insert(raw, idx);
        return Ok(idx);
    }

    // DataView.
    if let Some(dv) = value.as_data_view() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let byte_offset = dv.byte_offset(ctx.heap());
        let byte_length = dv.byte_length(ctx.heap());
        let bytes = dv.buffer(ctx.heap()).with_bytes(ctx.heap(), |b| b.to_vec());
        let buffer = ctx
            .array_buffer_from_bytes_rooted(bytes, &[], &[])
            .map_err(|e| type_error(e.to_string()))?;
        let buffer_idx = push_root(ctx, Value::array_buffer(buffer));
        let ctor_val = ctor(ctx, "DataView")?;
        let buffer_val = read_root(ctx, buffer_idx);
        let instance = ctx.construct(
            ctor_val,
            &[
                buffer_val,
                number_value(byte_offset as f64),
                number_value(byte_length as f64),
            ],
        )?;
        let idx = push_root(ctx, instance);
        memo.insert(raw, idx);
        return Ok(idx);
    }

    // RegExp.
    if let Some(re) = value.as_regexp() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let source = re.source(ctx.heap());
        let flags = re.flags(ctx.heap()).to_js_string();
        let last_index = re.last_index(ctx.heap());
        let source_val = string_value(ctx, &source)?;
        let flags_val = string_value(ctx, &flags)?;
        let ctor_val = ctor(ctx, "RegExp")?;
        let instance = ctx.construct(ctor_val, &[source_val, flags_val])?;
        if let Some(obj) = instance.as_object() {
            object::set(
                obj,
                ctx.heap_mut(),
                "lastIndex",
                number_value(last_index as f64),
            );
        }
        let idx = push_root(ctx, instance);
        memo.insert(raw, idx);
        return Ok(idx);
    }

    // Array.
    if let Some(arr) = value.as_array() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let len = array::len(arr, ctx.heap());
        let new_arr = ctx
            .array_from_elements(std::iter::empty())
            .map_err(|e| type_error(e.to_string()))?;
        let arr_idx = push_root(ctx, Value::array(new_arr));
        memo.insert(raw, arr_idx);
        for i in 0..len {
            let element = array::get(arr, ctx.heap(), i);
            let child_idx = clone_to_index(ctx, element, memo)?;
            let child = read_root(ctx, child_idx);
            let live = read_root(ctx, arr_idx)
                .as_array()
                .ok_or_else(|| type_error("array clone relocated to non-array".to_string()))?;
            array::set(live, ctx.heap_mut(), i, child).map_err(|e| type_error(e.to_string()))?;
        }
        return Ok(arr_idx);
    }

    // Map.
    if let Some(map) = value.as_map() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let entries = collections::map_entries(map, ctx.heap());
        let new_map = ctx.alloc_map().map_err(|e| type_error(e.to_string()))?;
        let map_idx = push_root(ctx, Value::map(new_map));
        memo.insert(raw, map_idx);
        for (key, val) in entries {
            let key_idx = clone_to_index(ctx, key, memo)?;
            let val_idx = clone_to_index(ctx, val, memo)?;
            let key_v = read_root(ctx, key_idx);
            let val_v = read_root(ctx, val_idx);
            let mut live = read_root(ctx, map_idx)
                .as_map()
                .ok_or_else(|| type_error("map clone relocated".to_string()))?;
            ctx.map_set(&mut live, key_v, val_v)
                .map_err(|e| type_error(e.to_string()))?;
        }
        return Ok(map_idx);
    }

    // Set.
    if let Some(set) = value.as_set() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }
        let values = collections::set_values(set, ctx.heap());
        let new_set = ctx.alloc_set().map_err(|e| type_error(e.to_string()))?;
        let set_idx = push_root(ctx, Value::set(new_set));
        memo.insert(raw, set_idx);
        for val in values {
            let v_idx = clone_to_index(ctx, val, memo)?;
            let v = read_root(ctx, v_idx);
            let mut live = read_root(ctx, set_idx)
                .as_set()
                .ok_or_else(|| type_error("set clone relocated".to_string()))?;
            ctx.set_add(&mut live, v)
                .map_err(|e| type_error(e.to_string()))?;
        }
        return Ok(set_idx);
    }

    // Object — Date, Error, or a plain object.
    if let Some(obj) = value.as_object() {
        if let Some(&idx) = memo.get(&raw) {
            return Ok(idx);
        }

        if let Some(ms) = object::date_data(obj, ctx.heap()) {
            let ctor_val = ctor(ctx, "Date")?;
            let instance = ctx.construct(ctor_val, &[number_value(ms)])?;
            let idx = push_root(ctx, instance);
            memo.insert(raw, idx);
            return Ok(idx);
        }

        if let Some(error_name) = error_class_name(ctx, obj) {
            return clone_error(ctx, obj, &error_name, raw, memo);
        }

        // Plain object: own enumerable string-keyed data properties.
        let new_obj = ctx.alloc_object().map_err(|e| type_error(e.to_string()))?;
        let obj_idx = push_root(ctx, Value::object(new_obj));
        memo.insert(raw, obj_idx);
        let props: Vec<(String, Value)> = object::with_properties(obj, ctx.heap(), |p| {
            p.enumerable_data_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect()
        });
        for (key, val) in props {
            let child_idx = clone_to_index(ctx, val, memo)?;
            let child = read_root(ctx, child_idx);
            let live = read_root(ctx, obj_idx)
                .as_object()
                .ok_or_else(|| type_error("object clone relocated".to_string()))?;
            object::set(live, ctx.heap_mut(), &key, child);
        }
        return Ok(obj_idx);
    }

    Err(data_clone_error("value"))
}

fn clone_error(
    ctx: &mut NativeCtx<'_>,
    obj: otter_vm::object::JsObject,
    name: &str,
    raw: RawGc,
    memo: &mut HashMap<RawGc, usize>,
) -> Result<usize, NativeError> {
    let message = match object::get(obj, ctx.heap(), "message") {
        Some(v) if v.as_string(ctx.heap()).is_some() => {
            v.as_string(ctx.heap()).unwrap().to_lossy_string(ctx.heap())
        }
        _ => String::new(),
    };
    let message_val = string_value(ctx, &message)?;
    let ctor_name = match name {
        "EvalError" | "RangeError" | "ReferenceError" | "SyntaxError" | "TypeError"
        | "URIError" => name,
        _ => "Error",
    };
    let ctor_val = ctor(ctx, ctor_name)?;
    let instance = ctx.construct(ctor_val, &[message_val])?;
    let idx = push_root(ctx, instance);
    memo.insert(raw, idx);
    // Clone the `cause` if present.
    if let Some(cause) = object::get(obj, ctx.heap(), "cause") {
        let cause_idx = clone_to_index(ctx, cause, memo)?;
        let cause_v = read_root(ctx, cause_idx);
        if let Some(inst) = read_root(ctx, idx).as_object() {
            object::set(inst, ctx.heap_mut(), "cause", cause_v);
        }
    }
    Ok(idx)
}

fn error_class_name(ctx: &NativeCtx<'_>, obj: otter_vm::object::JsObject) -> Option<String> {
    let name = match object::get(obj, ctx.heap(), "name") {
        Some(v) => v.as_string(ctx.heap())?.to_lossy_string(ctx.heap()),
        None => return None,
    };
    otter_vm::error_classes::ErrorKind::from_class_name(&name)?;
    Some(name)
}

fn typed_array_ctor_name(kind: TypedArrayKind) -> &'static str {
    match kind {
        TypedArrayKind::Int8 => "Int8Array",
        TypedArrayKind::Uint8 => "Uint8Array",
        TypedArrayKind::Uint8Clamped => "Uint8ClampedArray",
        TypedArrayKind::Int16 => "Int16Array",
        TypedArrayKind::Uint16 => "Uint16Array",
        TypedArrayKind::Int32 => "Int32Array",
        TypedArrayKind::Uint32 => "Uint32Array",
        TypedArrayKind::Float32 => "Float32Array",
        TypedArrayKind::Float64 => "Float64Array",
        TypedArrayKind::BigInt64 => "BigInt64Array",
        TypedArrayKind::BigUint64 => "BigUint64Array",
        TypedArrayKind::Float16 => "Float16Array",
    }
}

fn string_value(ctx: &mut NativeCtx<'_>, s: &str) -> Result<Value, NativeError> {
    otter_vm::string::JsString::from_str(s, ctx.heap_mut())
        .map(Value::string)
        .map_err(|e| type_error(e.to_string()))
}
