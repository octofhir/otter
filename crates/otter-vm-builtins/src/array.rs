//! Array built-in
//!
//! Provides Array static methods and prototype methods per ECMAScript 2024/2025.

use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value as VmValue;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Array constructor and methods
pub struct ArrayBuiltin;

/// Get Array ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // All ops now use native Value (no JSON conversion)
        // Static methods
        op_native("__Array_isArray", native_array_is_array),
        op_native("__Array_from", native_array_from),
        op_native("__Array_of", native_array_of),
        // Mutating methods
        op_native("__Array_push", native_array_push),
        op_native("__Array_pop", native_array_pop),
        op_native("__Array_shift", native_array_shift),
        op_native("__Array_unshift", native_array_unshift),
        op_native("__Array_splice", native_array_splice),
        op_native("__Array_reverse", native_array_reverse),
        op_native("__Array_sort", native_array_sort),
        op_native("__Array_fill", native_array_fill),
        op_native("__Array_copyWithin", native_array_copy_within),
        // Non-mutating methods
        op_native("__Array_slice", native_array_slice),
        op_native("__Array_concat", native_array_concat),
        op_native("__Array_flat", native_array_flat),
        op_native("__Array_flatMap", native_array_flat_map),
        // Search methods
        op_native("__Array_indexOf", native_array_index_of),
        op_native("__Array_lastIndexOf", native_array_last_index_of),
        op_native("__Array_includes", native_array_includes),
        op_native("__Array_find", native_array_find),
        op_native("__Array_findIndex", native_array_find_index),
        op_native("__Array_findLast", native_array_find_last),
        op_native("__Array_findLastIndex", native_array_find_last_index),
        op_native("__Array_at", native_array_at),
        // Iteration methods (simplified - callback support in builtins.js)
        op_native("__Array_forEach", native_array_for_each),
        op_native("__Array_map", native_array_map),
        op_native("__Array_filter", native_array_filter),
        op_native("__Array_reduce", native_array_reduce),
        op_native("__Array_reduceRight", native_array_reduce_right),
        op_native("__Array_every", native_array_every),
        op_native("__Array_some", native_array_some),
        // Conversion methods
        op_native("__Array_join", native_array_join),
        op_native("__Array_toString", native_array_to_string),
        op_native("__Array_length", native_array_length),
        // Native push (for internal use)
        op_native("__Array_push_native", native_array_push),
        // ES2023 immutable methods
        op_native("__Array_toReversed", native_array_to_reversed),
        op_native("__Array_toSorted", native_array_to_sorted),
        op_native("__Array_toSpliced", native_array_to_spliced),
        op_native("__Array_with", native_array_with),
    ]
}

// =============================================================================
// Helper functions
// =============================================================================

fn get_array_length(arr: &GcRef<JsObject>) -> usize {
    arr.get(&PropertyKey::string("length"))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as usize
}

fn set_array_length(arr: &GcRef<JsObject>, len: usize) {
    let _ = arr.set(PropertyKey::string("length"), VmValue::number(len as f64));
}

fn value_to_string(v: &VmValue) -> String {
    if let Some(s) = v.as_string() {
        s.as_str().to_string()
    } else if let Some(n) = v.as_number() {
        if n.is_nan() {
            "NaN".to_string()
        } else if n.is_infinite() {
            if n.is_sign_positive() {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            }
        } else {
            n.to_string()
        }
    } else if let Some(b) = v.as_boolean() {
        b.to_string()
    } else if v.is_null() {
        "null".to_string()
    } else if v.is_undefined() {
        "undefined".to_string()
    } else {
        "[object]".to_string()
    }
}

// =============================================================================
// Static Methods
// =============================================================================

fn native_array_is_array(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let val = args.get(0);
    let is_array = val
        .and_then(|v| v.as_object())
        .map(|o| o.is_array())
        .unwrap_or(false);
    Ok(VmValue::boolean(is_array))
}

fn native_array_from(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let val = args.get(0).ok_or("Array.from requires an argument")?;

    if let Some(arr) = val.as_object() {
        if arr.is_array() {
            // Clone array elements
            let len = get_array_length(&arr);
            let new_arr = GcRef::new(JsObject::array(len, Arc::clone(&mm)));
            for i in 0..len {
                if let Some(elem) = arr.get(&PropertyKey::Index(i as u32)) {
                    let _ = new_arr.set(PropertyKey::Index(i as u32), elem);
                }
            }
            return Ok(VmValue::array(new_arr));
        }
    }

    if let Some(s) = val.as_string() {
        // Convert string to array of characters
        let chars: Vec<_> = s.as_str().chars().collect();
        let arr = GcRef::new(JsObject::array(chars.len(), Arc::clone(&mm)));
        for (i, ch) in chars.into_iter().enumerate() {
            let _ = arr.set(
                PropertyKey::Index(i as u32),
                VmValue::string(JsString::intern(&ch.to_string())),
            );
        }
        return Ok(VmValue::array(arr));
    }

    // Return empty array for other types
    Ok(VmValue::array(GcRef::new(JsObject::array(0, mm))))
}

fn native_array_of(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr = GcRef::new(JsObject::array(args.len(), Arc::clone(&mm)));
    for (i, item) in args.iter().enumerate() {
        let _ = arr.set(PropertyKey::Index(i as u32), item.clone());
    }
    Ok(VmValue::array(arr))
}

// =============================================================================
// Mutating Methods
// =============================================================================

fn native_array_push(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.push requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.push called on non-object")?;

    let len = get_array_length(&arr);

    // Items to push are args[1..]
    for (i, item) in args[1..].iter().enumerate() {
        let _ = arr.set(PropertyKey::Index((len + i) as u32), item.clone());
    }

    let new_len = len + args.len() - 1;
    set_array_length(&arr, new_len);
    Ok(VmValue::number(new_len as f64))
}

fn native_array_pop(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.pop requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.pop called on non-object")?;

    let len = get_array_length(&arr);
    if len == 0 {
        return Ok(VmValue::undefined());
    }

    let last = arr
        .get(&PropertyKey::Index((len - 1) as u32))
        .unwrap_or(VmValue::undefined());
    arr.delete(&PropertyKey::Index((len - 1) as u32));
    set_array_length(&arr, len - 1);
    Ok(last)
}

fn native_array_shift(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.shift requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.shift called on non-object")?;

    let len = get_array_length(&arr);
    if len == 0 {
        return Ok(VmValue::undefined());
    }

    let first = arr
        .get(&PropertyKey::Index(0))
        .unwrap_or(VmValue::undefined());

    // Shift all elements down
    for i in 1..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = arr.set(PropertyKey::Index((i - 1) as u32), val);
        } else {
            arr.delete(&PropertyKey::Index((i - 1) as u32));
        }
    }

    arr.delete(&PropertyKey::Index((len - 1) as u32));
    set_array_length(&arr, len - 1);
    Ok(first)
}

fn native_array_unshift(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.unshift requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.unshift called on non-object")?;

    let len = get_array_length(&arr);
    let items = &args[1..];
    let items_len = items.len();

    // Shift existing elements up
    for i in (0..len).rev() {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = arr.set(PropertyKey::Index((i + items_len) as u32), val);
        }
    }

    // Insert new items at the beginning
    for (i, item) in items.iter().enumerate() {
        let _ = arr.set(PropertyKey::Index(i as u32), item.clone());
    }

    let new_len = len + items_len;
    set_array_length(&arr, new_len);
    Ok(VmValue::number(new_len as f64))
}

fn native_array_splice(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.splice requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.splice called on non-object")?;

    let len = get_array_length(&arr) as i32;
    let start = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let delete_count = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as usize)
        .unwrap_or(len.saturating_sub(start) as usize);

    let start = if start < 0 {
        (len + start).max(0) as usize
    } else {
        (start as usize).min(len as usize)
    };

    let items = &args[3..];
    let items_len = items.len();

    // Collect deleted elements
    let deleted = GcRef::new(JsObject::array(delete_count, Arc::clone(&mm)));
    for i in 0..delete_count.min(len as usize - start) {
        if let Some(val) = arr.get(&PropertyKey::Index((start + i) as u32)) {
            let _ = deleted.set(PropertyKey::Index(i as u32), val);
        }
    }

    // Calculate size change
    let actual_delete = delete_count.min(len as usize - start);
    let size_change = items_len as i32 - actual_delete as i32;

    if size_change != 0 {
        if size_change > 0 {
            // Shift elements right
            for i in (start + actual_delete..len as usize).rev() {
                if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
                    let _ = arr.set(PropertyKey::Index((i as i32 + size_change) as u32), val);
                }
            }
        } else {
            // Shift elements left
            for i in start + actual_delete..len as usize {
                if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
                    let _ = arr.set(PropertyKey::Index((i as i32 + size_change) as u32), val);
                } else {
                    arr.delete(&PropertyKey::Index((i as i32 + size_change) as u32));
                }
            }
        }
    }

    // Insert new items
    for (i, item) in items.iter().enumerate() {
        let _ = arr.set(PropertyKey::Index((start + i) as u32), item.clone());
    }

    let new_len = (len as usize as i32 + size_change) as usize;
    set_array_length(&arr, new_len);
    Ok(VmValue::array(deleted))
}

fn native_array_reverse(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.reverse requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.reverse called on non-object")?;

    let len = get_array_length(&arr);
    for i in 0..len / 2 {
        let j = len - 1 - i;
        let a = arr.get(&PropertyKey::Index(i as u32));
        let b = arr.get(&PropertyKey::Index(j as u32));

        match (a, b) {
            (Some(val_a), Some(val_b)) => {
                let _ = arr.set(PropertyKey::Index(i as u32), val_b);
                let _ = arr.set(PropertyKey::Index(j as u32), val_a);
            }
            (Some(val_a), None) => {
                arr.delete(&PropertyKey::Index(i as u32));
                let _ = arr.set(PropertyKey::Index(j as u32), val_a);
            }
            (None, Some(val_b)) => {
                let _ = arr.set(PropertyKey::Index(i as u32), val_b);
                arr.delete(&PropertyKey::Index(j as u32));
            }
            (None, None) => {}
        }
    }

    Ok(arr_val.clone())
}

fn native_array_sort(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.sort requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.sort called on non-object")?;

    let len = get_array_length(&arr);
    let mut elements: Vec<(usize, VmValue)> = Vec::new();

    for i in 0..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            elements.push((i, val));
        }
    }

    // Sort lexicographically
    elements.sort_by(|(_i, a), (_j, b)| {
        let a_str = value_to_string(a);
        let b_str = value_to_string(b);
        a_str.cmp(&b_str)
    });

    // Write back sorted elements
    for (new_i, (_old_i, val)) in elements.into_iter().enumerate() {
        let _ = arr.set(PropertyKey::Index(new_i as u32), val);
    }

    Ok(arr_val.clone())
}

fn native_array_fill(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.fill requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.fill called on non-object")?;

    let value = args.get(1).cloned().unwrap_or(VmValue::undefined());
    let len = get_array_length(&arr) as i32;

    let start = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as i32)
        .unwrap_or(0);
    let end = args
        .get(3)
        .and_then(|v| v.as_number())
        .map(|n| n as i32)
        .unwrap_or(len);

    let start = if start < 0 {
        (len + start).max(0) as usize
    } else {
        (start as usize).min(len as usize)
    };

    let end = if end < 0 {
        (len + end).max(0) as usize
    } else {
        (end as usize).min(len as usize)
    };

    for i in start..end {
        let _ = arr.set(PropertyKey::Index(i as u32), value.clone());
    }

    Ok(arr_val.clone())
}

fn native_array_copy_within(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.copyWithin requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.copyWithin called on non-object")?;

    let len = get_array_length(&arr) as i32;
    let target = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let start = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let end = args
        .get(3)
        .and_then(|v| v.as_number())
        .map(|n| n as i32)
        .unwrap_or(len);

    let target = if target < 0 {
        (len + target).max(0) as usize
    } else {
        (target as usize).min(len as usize)
    };

    let start = if start < 0 {
        (len + start).max(0) as usize
    } else {
        (start as usize).min(len as usize)
    };

    let end = if end < 0 {
        (len + end).max(0) as usize
    } else {
        (end as usize).min(len as usize)
    };

    let count = (end.saturating_sub(start)).min(len as usize - target);

    // Copy elements to temporary storage
    let mut temp = Vec::new();
    for i in 0..count {
        temp.push(arr.get(&PropertyKey::Index((start + i) as u32)));
    }

    // Write them to target
    for (i, val_opt) in temp.into_iter().enumerate() {
        if let Some(val) = val_opt {
            let _ = arr.set(PropertyKey::Index((target + i) as u32), val);
        } else {
            arr.delete(&PropertyKey::Index((target + i) as u32));
        }
    }

    Ok(arr_val.clone())
}

// =============================================================================
// Non-mutating Methods
// =============================================================================

fn native_array_slice(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.slice requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.slice called on non-object")?;

    let len = get_array_length(&arr) as i32;
    let start = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let end = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as i32)
        .unwrap_or(len);

    let start = if start < 0 {
        (len + start).max(0) as usize
    } else {
        (start as usize).min(len as usize)
    };

    let end = if end < 0 {
        (len + end).max(0) as usize
    } else {
        (end as usize).min(len as usize)
    };

    let count = end.saturating_sub(start);
    let new_arr = GcRef::new(JsObject::array(count, Arc::clone(&mm)));

    for i in 0..count {
        if let Some(val) = arr.get(&PropertyKey::Index((start + i) as u32)) {
            let _ = new_arr.set(PropertyKey::Index(i as u32), val);
        }
    }

    Ok(VmValue::array(new_arr))
}

fn native_array_concat(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.concat requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.concat called on non-object")?;

    let new_arr = GcRef::new(JsObject::array(0, Arc::clone(&mm)));
    let mut new_len = 0;

    // Copy original array
    let len = get_array_length(&arr);
    for i in 0..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = new_arr.set(PropertyKey::Index(new_len as u32), val);
            new_len += 1;
        }
    }

    // Concat additional arguments
    for arg in &args[1..] {
        if let Some(concat_arr) = arg.as_object() {
            if concat_arr.is_array() {
                let concat_len = get_array_length(&concat_arr);
                for i in 0..concat_len {
                    if let Some(val) = concat_arr.get(&PropertyKey::Index(i as u32)) {
                        let _ = new_arr.set(PropertyKey::Index(new_len as u32), val);
                        new_len += 1;
                    }
                }
                continue;
            }
        }
        // Non-array argument, add as single element
        let _ = new_arr.set(PropertyKey::Index(new_len as u32), arg.clone());
        new_len += 1;
    }

    set_array_length(&new_arr, new_len);
    Ok(VmValue::array(new_arr))
}

fn native_array_flat(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.flat requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.flat called on non-object")?;

    let depth = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(1.0) as usize;

    let new_arr = GcRef::new(JsObject::array(0, Arc::clone(&mm)));
    let mut new_len = 0;

    fn flatten(
        arr: &GcRef<JsObject>,
        result: &GcRef<JsObject>,
        result_len: &mut usize,
        depth: usize,
    ) {
        let len = get_array_length(arr);
        for i in 0..len {
            if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
                if depth > 0 {
                    if let Some(sub_arr) = val.as_object() {
                        if sub_arr.is_array() {
                            flatten(&sub_arr, result, result_len, depth - 1);
                            continue;
                        }
                    }
                }
                let _ = result.set(PropertyKey::Index(*result_len as u32), val);
                *result_len += 1;
            }
        }
    }

    flatten(&arr, &new_arr, &mut new_len, depth);
    set_array_length(&new_arr, new_len);
    Ok(VmValue::array(new_arr))
}

fn native_array_flat_map(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // Simplified: flatMap requires callback, which is handled in builtins.js
    Err(VmError::type_error("Array.flatMap not yet implemented in native ops"))
}

// =============================================================================
// Search Methods
// =============================================================================

fn native_array_index_of(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.indexOf requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.indexOf called on non-object")?;

    let search_element = args.get(1).cloned().unwrap_or(VmValue::undefined());
    let from_index = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;

    let len = get_array_length(&arr) as i32;
    let start = if from_index < 0 {
        (len + from_index).max(0) as usize
    } else {
        (from_index as usize).min(len as usize)
    };

    for i in start..len as usize {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            // Strict equality check
            if values_equal(&val, &search_element) {
                return Ok(VmValue::int32(i as i32));
            }
        }
    }

    Ok(VmValue::int32(-1))
}

fn native_array_last_index_of(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.lastIndexOf requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.lastIndexOf called on non-object")?;

    let search_element = args.get(1).cloned().unwrap_or(VmValue::undefined());
    let len = get_array_length(&arr) as i32;
    let from_index = args
        .get(2)
        .and_then(|v| v.as_number())
        .map(|n| n as i32)
        .unwrap_or(len - 1);

    let start = if from_index < 0 {
        (len + from_index).max(0) as usize
    } else {
        (from_index as usize).min((len - 1) as usize)
    };

    for i in (0..=start).rev() {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            if values_equal(&val, &search_element) {
                return Ok(VmValue::int32(i as i32));
            }
        }
    }

    Ok(VmValue::int32(-1))
}

fn native_array_includes(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.includes requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.includes called on non-object")?;

    let search_element = args.get(1).cloned().unwrap_or(VmValue::undefined());
    let from_index = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;

    let len = get_array_length(&arr) as i32;
    let start = if from_index < 0 {
        (len + from_index).max(0) as usize
    } else {
        (from_index as usize).min(len as usize)
    };

    for i in start..len as usize {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            // includes uses SameValueZero (NaN == NaN)
            if values_equal_same_value_zero(&val, &search_element) {
                return Ok(VmValue::boolean(true));
            }
        }
    }

    Ok(VmValue::boolean(false))
}

fn native_array_find(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // Requires callback, handled in builtins.js
    Err(VmError::type_error("Array.find not yet implemented in native ops"))
}

fn native_array_find_index(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // Requires callback, handled in builtins.js
    Err(VmError::type_error("Array.findIndex not yet implemented in native ops"))
}

fn native_array_find_last(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // Requires callback, handled in builtins.js
    Err(VmError::type_error("Array.findLast not yet implemented in native ops"))
}

fn native_array_find_last_index(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // Requires callback, handled in builtins.js
    Err(VmError::type_error("Array.findLastIndex not yet implemented in native ops"))
}

fn native_array_at(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.at requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.at called on non-object")?;

    let index = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;

    let len = get_array_length(&arr) as i32;
    let actual_index = if index < 0 {
        len + index
    } else {
        index
    };

    if actual_index < 0 || actual_index >= len {
        return Ok(VmValue::undefined());
    }

    Ok(arr
        .get(&PropertyKey::Index(actual_index as u32))
        .unwrap_or(VmValue::undefined()))
}

// =============================================================================
// Iteration Methods (callback-based, simplified)
// =============================================================================

fn native_array_for_each(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.forEach not yet implemented in native ops"))
}

fn native_array_map(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.map not yet implemented in native ops"))
}

fn native_array_filter(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.filter not yet implemented in native ops"))
}

fn native_array_reduce(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.reduce not yet implemented in native ops"))
}

fn native_array_reduce_right(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.reduceRight not yet implemented in native ops"))
}

fn native_array_every(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.every not yet implemented in native ops"))
}

fn native_array_some(_args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    Err(VmError::type_error("Array.some not yet implemented in native ops"))
}

// =============================================================================
// Conversion Methods
// =============================================================================

fn native_array_join(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.join requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.join called on non-object")?;

    let separator = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| ",".to_string());

    let len = get_array_length(&arr);
    let mut parts = Vec::new();

    for i in 0..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            if !val.is_null() && !val.is_undefined() {
                parts.push(value_to_string(&val));
            } else {
                parts.push(String::new());
            }
        } else {
            parts.push(String::new());
        }
    }

    Ok(VmValue::string(JsString::intern(&parts.join(&separator))))
}

fn native_array_to_string(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    // toString() just calls join()
    native_array_join(args, mm)
}

fn native_array_length(args: &[VmValue], _mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.length requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.length called on non-object")?;

    let len = get_array_length(&arr);
    Ok(VmValue::number(len as f64))
}

// =============================================================================
// ES2023 Immutable Methods
// =============================================================================

fn native_array_to_reversed(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.toReversed requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.toReversed called on non-object")?;

    let len = get_array_length(&arr);
    let new_arr = GcRef::new(JsObject::array(len, Arc::clone(&mm)));

    for i in 0..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = new_arr.set(PropertyKey::Index((len - 1 - i) as u32), val);
        }
    }

    Ok(VmValue::array(new_arr))
}

fn native_array_to_sorted(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.toSorted requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.toSorted called on non-object")?;

    let len = get_array_length(&arr);
    let new_arr = GcRef::new(JsObject::array(len, Arc::clone(&mm)));

    // Copy elements
    for i in 0..len {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = new_arr.set(PropertyKey::Index(i as u32), val);
        }
    }

    // Sort the copy
    let mut elements: Vec<(usize, VmValue)> = Vec::new();
    for i in 0..len {
        if let Some(val) = new_arr.get(&PropertyKey::Index(i as u32)) {
            elements.push((i, val));
        }
    }

    elements.sort_by(|(_i, a), (_j, b)| {
        let a_str = value_to_string(a);
        let b_str = value_to_string(b);
        a_str.cmp(&b_str)
    });

    for (new_i, (_old_i, val)) in elements.into_iter().enumerate() {
        let _ = new_arr.set(PropertyKey::Index(new_i as u32), val);
    }

    Ok(VmValue::array(new_arr))
}

fn native_array_to_spliced(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.toSpliced requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.toSpliced called on non-object")?;

    let len = get_array_length(&arr) as i32;
    let start = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let delete_count = args
        .get(2)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as usize;

    let start = if start < 0 {
        (len + start).max(0) as usize
    } else {
        (start as usize).min(len as usize)
    };

    let items = &args[3..];
    let new_len = len as usize - delete_count.min(len as usize - start) + items.len();
    let new_arr = GcRef::new(JsObject::array(new_len, Arc::clone(&mm)));

    let mut new_i = 0;

    // Copy elements before start
    for i in 0..start {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = new_arr.set(PropertyKey::Index(new_i as u32), val);
        }
        new_i += 1;
    }

    // Insert new items
    for item in items {
        let _ = new_arr.set(PropertyKey::Index(new_i as u32), item.clone());
        new_i += 1;
    }

    // Copy elements after deleted range
    for i in start + delete_count..len as usize {
        if let Some(val) = arr.get(&PropertyKey::Index(i as u32)) {
            let _ = new_arr.set(PropertyKey::Index(new_i as u32), val);
        }
        new_i += 1;
    }

    Ok(VmValue::array(new_arr))
}

fn native_array_with(args: &[VmValue], mm: Arc<MemoryManager>) -> Result<VmValue, VmError> {
    let arr_val = args.get(0).ok_or("Array.with requires a target")?;
    let arr = arr_val
        .as_object()
        .ok_or("Array.with called on non-object")?;

    let index = args
        .get(1)
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as i32;
    let value = args.get(2).cloned().unwrap_or(VmValue::undefined());

    let len = get_array_length(&arr) as i32;
    let actual_index = if index < 0 { len + index } else { index };

    if actual_index < 0 || actual_index >= len {
        return Err(VmError::range_error(format!("Index {} out of bounds for array of length {}", index, len)));
    }

    let new_arr = GcRef::new(JsObject::array(len as usize, Arc::clone(&mm)));

    for i in 0..len {
        let val = if i == actual_index {
            value.clone()
        } else {
            arr.get(&PropertyKey::Index(i as u32))
                .unwrap_or(VmValue::undefined())
        };
        let _ = new_arr.set(PropertyKey::Index(i as u32), val);
    }

    Ok(VmValue::array(new_arr))
}

// =============================================================================
// Helper functions for value comparison
// =============================================================================

fn values_equal(a: &VmValue, b: &VmValue) -> bool {
    // Strict equality (===)
    if a.is_undefined() && b.is_undefined() {
        return true;
    }
    if a.is_null() && b.is_null() {
        return true;
    }
    if let (Some(na), Some(nb)) = (a.as_number(), b.as_number()) {
        if na.is_nan() || nb.is_nan() {
            return false; // NaN !== NaN
        }
        return na == nb;
    }
    if let (Some(ba), Some(bb)) = (a.as_boolean(), b.as_boolean()) {
        return ba == bb;
    }
    if let (Some(sa), Some(sb)) = (a.as_string(), b.as_string()) {
        return sa.as_str() == sb.as_str();
    }
    if let (Some(oa), Some(ob)) = (a.as_object(), b.as_object()) {
        return oa.as_ptr() == ob.as_ptr();
    }
    false
}

fn values_equal_same_value_zero(a: &VmValue, b: &VmValue) -> bool {
    // SameValueZero (NaN == NaN, but +0 == -0)
    if let (Some(na), Some(nb)) = (a.as_number(), b.as_number()) {
        if na.is_nan() && nb.is_nan() {
            return true; // NaN == NaN
        }
        return na == nb;
    }
    values_equal(a, b)
}
