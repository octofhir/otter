//! Array built-in
//!
//! Provides Array static methods and prototype methods per ECMAScript 2024/2025.

use otter_macros::dive;
use otter_vm_runtime::{Op, op_sync};
use serde_json::Value as JsonValue;

/// Array constructor and methods
pub struct ArrayBuiltin;

/// Get Array ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Static methods
        op_sync("__Array_isArray", __otter_dive_array_is_array),
        op_sync("__Array_from", __otter_dive_array_from),
        op_sync("__Array_of", __otter_dive_array_of),
        // Mutating methods
        op_sync("__Array_push", __otter_dive_array_push),
        op_sync("__Array_pop", __otter_dive_array_pop),
        op_sync("__Array_shift", __otter_dive_array_shift),
        op_sync("__Array_unshift", __otter_dive_array_unshift),
        op_sync("__Array_splice", __otter_dive_array_splice),
        op_sync("__Array_reverse", __otter_dive_array_reverse),
        op_sync("__Array_sort", __otter_dive_array_sort),
        op_sync("__Array_fill", __otter_dive_array_fill),
        op_sync("__Array_copyWithin", __otter_dive_array_copy_within),
        // Non-mutating methods
        op_sync("__Array_slice", __otter_dive_array_slice),
        op_sync("__Array_concat", __otter_dive_array_concat),
        op_sync("__Array_flat", __otter_dive_array_flat),
        op_sync("__Array_flatMap", __otter_dive_array_flat_map),
        // Search methods
        op_sync("__Array_indexOf", __otter_dive_array_index_of),
        op_sync("__Array_lastIndexOf", __otter_dive_array_last_index_of),
        op_sync("__Array_includes", __otter_dive_array_includes),
        op_sync("__Array_find", __otter_dive_array_find),
        op_sync("__Array_findIndex", __otter_dive_array_find_index),
        op_sync("__Array_findLast", __otter_dive_array_find_last),
        op_sync("__Array_findLastIndex", __otter_dive_array_find_last_index),
        op_sync("__Array_at", __otter_dive_array_at),
        // Iteration methods (simplified - no callback support in JSON ops)
        op_sync("__Array_forEach", __otter_dive_array_for_each),
        op_sync("__Array_map", __otter_dive_array_map),
        op_sync("__Array_filter", __otter_dive_array_filter),
        op_sync("__Array_reduce", __otter_dive_array_reduce),
        op_sync("__Array_reduceRight", __otter_dive_array_reduce_right),
        op_sync("__Array_every", __otter_dive_array_every),
        op_sync("__Array_some", __otter_dive_array_some),
        // Conversion methods
        op_sync("__Array_join", __otter_dive_array_join),
        op_sync("__Array_toString", __otter_dive_array_to_string),
        op_sync("__Array_length", __otter_dive_array_length),
        // ES2023 immutable methods
        op_sync("__Array_toReversed", __otter_dive_array_to_reversed),
        op_sync("__Array_toSorted", __otter_dive_array_to_sorted),
        op_sync("__Array_toSpliced", __otter_dive_array_to_spliced),
        op_sync("__Array_with", __otter_dive_array_with),
    ]
}

// ============================================================================
// Static Methods
// ============================================================================

/// Array.isArray() - returns true if the value is an array
#[dive(swift)]
#[allow(dead_code)]
fn array_is_array(val: JsonValue) -> bool {
    val.is_array()
}

/// Array.from() - creates an array from an array-like or iterable
#[dive(swift)]
#[allow(dead_code)]
fn array_from(val: JsonValue) -> Vec<JsonValue> {
    match val {
        JsonValue::Array(arr) => arr,
        JsonValue::String(s) => s
            .chars()
            .map(|c| JsonValue::String(c.to_string()))
            .collect(),
        _ => vec![],
    }
}

/// Array.of() - creates an array from its arguments
#[dive(swift)]
#[allow(dead_code)]
fn array_of(items: Vec<JsonValue>) -> Vec<JsonValue> {
    items
}

// ============================================================================
// Mutating Methods
// ============================================================================

/// Array.prototype.push() - adds elements to end, returns new length
#[dive(swift)]
#[allow(dead_code)]
fn array_push(arr: JsonValue, items: Vec<JsonValue>) -> Result<usize, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(existing.len() + items.len())
}

/// Array.prototype.pop() - removes and returns last element
#[dive(swift)]
#[allow(dead_code)]
fn array_pop(arr: JsonValue) -> Result<JsonValue, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(existing.last().cloned().unwrap_or(JsonValue::Null))
}

/// Array.prototype.shift() - removes and returns first element
#[dive(swift)]
#[allow(dead_code)]
fn array_shift(arr: JsonValue) -> Result<JsonValue, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(existing.first().cloned().unwrap_or(JsonValue::Null))
}

/// Array.prototype.unshift() - adds elements to beginning, returns new length
#[dive(swift)]
#[allow(dead_code)]
fn array_unshift(arr: JsonValue, items: Vec<JsonValue>) -> Result<usize, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(items.len() + existing.len())
}

/// Arguments for Array.splice
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SpliceArgs {
    pub arr: JsonValue,
    pub start: i64,
    pub delete_count: Option<usize>,
    pub items: Option<Vec<JsonValue>>,
}

/// Array.prototype.splice() - adds/removes elements
#[dive(swift)]
#[allow(dead_code)]
fn array_splice(args: SpliceArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;

    let start = if args.start < 0 {
        (len + args.start).max(0) as usize
    } else {
        (args.start as usize).min(arr.len())
    };

    let delete_count = args.delete_count.unwrap_or(arr.len() - start);
    let end = (start + delete_count).min(arr.len());

    // Return deleted elements
    Ok(arr[start..end].to_vec())
}

/// Array.prototype.reverse() - reverses array in place
#[dive(swift)]
#[allow(dead_code)]
fn array_reverse(arr: JsonValue) -> Result<Vec<JsonValue>, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    let mut result = existing.clone();
    result.reverse();
    Ok(result)
}

/// Array.prototype.sort() - sorts array (default: lexicographic)
#[dive(swift)]
#[allow(dead_code)]
fn array_sort(arr: JsonValue) -> Result<Vec<JsonValue>, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    let mut result = existing.clone();
    result.sort_by(|a, b| {
        let a_str = json_to_sort_string(a);
        let b_str = json_to_sort_string(b);
        a_str.cmp(&b_str)
    });
    Ok(result)
}

fn json_to_sort_string(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Bool(b) => b.to_string(),
        JsonValue::Null => "null".to_string(),
        _ => String::new(),
    }
}

/// Arguments for Array.fill
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FillArgs {
    pub arr: JsonValue,
    pub value: JsonValue,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Array.prototype.fill() - fills array with a value
#[dive(swift)]
#[allow(dead_code)]
fn array_fill(args: FillArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;
    let mut result = arr.clone();

    let start = match args.start {
        Some(s) if s < 0 => (len + s).max(0) as usize,
        Some(s) => (s as usize).min(arr.len()),
        None => 0,
    };

    let end = match args.end {
        Some(e) if e < 0 => (len + e).max(0) as usize,
        Some(e) => (e as usize).min(arr.len()),
        None => arr.len(),
    };

    for item in result.iter_mut().take(end).skip(start) {
        *item = args.value.clone();
    }

    Ok(result)
}

/// Arguments for Array.copyWithin
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CopyWithinArgs {
    pub arr: JsonValue,
    pub target: i64,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Array.prototype.copyWithin() - copies part of array to another location
#[dive(swift)]
#[allow(dead_code)]
fn array_copy_within(args: CopyWithinArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;
    let mut result = arr.clone();

    let target = if args.target < 0 {
        (len + args.target).max(0) as usize
    } else {
        (args.target as usize).min(arr.len())
    };

    let start = match args.start {
        Some(s) if s < 0 => (len + s).max(0) as usize,
        Some(s) => (s as usize).min(arr.len()),
        None => 0,
    };

    let end = match args.end {
        Some(e) if e < 0 => (len + e).max(0) as usize,
        Some(e) => (e as usize).min(arr.len()),
        None => arr.len(),
    };

    let count = (end - start).min(arr.len() - target);
    let source: Vec<_> = arr[start..start + count].to_vec();

    for (i, val) in source.into_iter().enumerate() {
        result[target + i] = val;
    }

    Ok(result)
}

// ============================================================================
// Non-Mutating Methods
// ============================================================================

/// Arguments for Array.slice
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SliceArgs {
    pub arr: JsonValue,
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// Array.prototype.slice() - returns a shallow copy of a portion
#[dive(swift)]
#[allow(dead_code)]
fn array_slice(args: SliceArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;

    let start = match args.start {
        Some(s) if s < 0 => (len + s).max(0) as usize,
        Some(s) => (s as usize).min(arr.len()),
        None => 0,
    };

    let end = match args.end {
        Some(e) if e < 0 => (len + e).max(0) as usize,
        Some(e) => (e as usize).min(arr.len()),
        None => arr.len(),
    };

    if start >= end {
        return Ok(vec![]);
    }

    Ok(arr[start..end].to_vec())
}

/// Array.prototype.concat() - merges arrays
#[dive(swift)]
#[allow(dead_code)]
fn array_concat(arr: JsonValue, items: Vec<JsonValue>) -> Result<Vec<JsonValue>, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    let mut result = existing.clone();

    for item in items {
        match item {
            JsonValue::Array(other) => result.extend(other),
            _ => result.push(item),
        }
    }

    Ok(result)
}

/// Arguments for Array.flat
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FlatArgs {
    pub arr: JsonValue,
    pub depth: Option<usize>,
}

/// Array.prototype.flat() - flattens nested arrays
#[dive(swift)]
#[allow(dead_code)]
fn array_flat(args: FlatArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let depth = args.depth.unwrap_or(1);
    Ok(flatten_array(arr, depth))
}

fn flatten_array(arr: &[JsonValue], depth: usize) -> Vec<JsonValue> {
    let mut result = Vec::new();
    for item in arr {
        if depth > 0 {
            if let JsonValue::Array(inner) = item {
                result.extend(flatten_array(inner, depth - 1));
            } else {
                result.push(item.clone());
            }
        } else {
            result.push(item.clone());
        }
    }
    result
}

/// Arguments for Array.flatMap - applies fn then flattens one level
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FlatMapArgs {
    pub arr: JsonValue,
    pub mapped: Vec<JsonValue>, // Pre-mapped results from JS
}

/// Array.prototype.flatMap() - map + flat(1)
#[dive(swift)]
#[allow(dead_code)]
fn array_flat_map(args: FlatMapArgs) -> Result<Vec<JsonValue>, String> {
    let mut result = Vec::new();
    for item in args.mapped {
        if let JsonValue::Array(inner) = item {
            result.extend(inner);
        } else {
            result.push(item);
        }
    }
    Ok(result)
}

// ============================================================================
// Search Methods
// ============================================================================

/// Array.prototype.indexOf() - returns first index of element
#[dive(swift)]
#[allow(dead_code)]
fn array_index_of(arr: JsonValue, search: JsonValue) -> Result<i64, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;

    for (i, item) in existing.iter().enumerate() {
        if item == &search {
            return Ok(i as i64);
        }
    }

    Ok(-1)
}

/// Array.prototype.lastIndexOf() - returns last index of element
#[dive(swift)]
#[allow(dead_code)]
fn array_last_index_of(arr: JsonValue, search: JsonValue) -> Result<i64, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;

    for (i, item) in existing.iter().enumerate().rev() {
        if item == &search {
            return Ok(i as i64);
        }
    }

    Ok(-1)
}

/// Array.prototype.includes() - returns true if array contains element
#[dive(swift)]
#[allow(dead_code)]
fn array_includes(arr: JsonValue, search: JsonValue) -> Result<bool, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(existing.contains(&search))
}

/// Arguments for find operations with pre-computed results from JS
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FindArgs {
    pub arr: JsonValue,
    pub results: Vec<bool>, // Pre-computed predicate results from JS
}

/// Array.prototype.find() - returns first element matching predicate
#[dive(swift)]
#[allow(dead_code)]
fn array_find(args: FindArgs) -> Result<JsonValue, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;

    for (i, result) in args.results.iter().enumerate() {
        if *result && i < arr.len() {
            return Ok(arr[i].clone());
        }
    }

    Ok(JsonValue::Null)
}

/// Array.prototype.findIndex() - returns index of first matching element
#[dive(swift)]
#[allow(dead_code)]
fn array_find_index(args: FindArgs) -> Result<i64, String> {
    for (i, result) in args.results.iter().enumerate() {
        if *result {
            return Ok(i as i64);
        }
    }
    Ok(-1)
}

/// Array.prototype.findLast() - returns last element matching predicate (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_find_last(args: FindArgs) -> Result<JsonValue, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;

    for (i, result) in args.results.iter().enumerate().rev() {
        if *result && i < arr.len() {
            return Ok(arr[i].clone());
        }
    }

    Ok(JsonValue::Null)
}

/// Array.prototype.findLastIndex() - returns index of last matching element (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_find_last_index(args: FindArgs) -> Result<i64, String> {
    for (i, result) in args.results.iter().enumerate().rev() {
        if *result {
            return Ok(i as i64);
        }
    }
    Ok(-1)
}

/// Array.prototype.at() - returns element at index (supports negative)
#[dive(swift)]
#[allow(dead_code)]
fn array_at(arr: JsonValue, index: i64) -> Result<JsonValue, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    let len = existing.len() as i64;

    let idx = if index < 0 { len + index } else { index };

    if idx < 0 || idx >= len {
        return Ok(JsonValue::Null);
    }

    Ok(existing[idx as usize].clone())
}

// ============================================================================
// Iteration Methods (simplified - callbacks evaluated in JS)
// ============================================================================

/// Array.prototype.forEach() - calls function for each element (returns undefined)
#[dive(swift)]
#[allow(dead_code)]
fn array_for_each(arr: JsonValue) -> Result<JsonValue, String> {
    let _ = arr.as_array().ok_or("First argument must be array")?;
    Ok(JsonValue::Null) // forEach returns undefined
}

/// Arguments for map with pre-computed results
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MapArgs {
    pub results: Vec<JsonValue>, // Pre-computed mapped values from JS
}

/// Array.prototype.map() - creates new array with results of callback
#[dive(swift)]
#[allow(dead_code)]
fn array_map(args: MapArgs) -> Vec<JsonValue> {
    args.results
}

/// Array.prototype.filter() - creates new array with elements passing test
#[dive(swift)]
#[allow(dead_code)]
fn array_filter(args: FindArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;

    let result: Vec<JsonValue> = arr
        .iter()
        .enumerate()
        .filter(|(i, _)| args.results.get(*i).copied().unwrap_or(false))
        .map(|(_, v)| v.clone())
        .collect();

    Ok(result)
}

/// Arguments for reduce
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReduceArgs {
    pub result: JsonValue, // Final accumulated value computed in JS
}

/// Array.prototype.reduce() - reduces array to single value (left to right)
#[dive(swift)]
#[allow(dead_code)]
fn array_reduce(args: ReduceArgs) -> JsonValue {
    args.result
}

/// Array.prototype.reduceRight() - reduces array to single value (right to left)
#[dive(swift)]
#[allow(dead_code)]
fn array_reduce_right(args: ReduceArgs) -> JsonValue {
    args.result
}

/// Array.prototype.every() - tests if all elements pass test
#[dive(swift)]
#[allow(dead_code)]
fn array_every(results: Vec<bool>) -> bool {
    results.iter().all(|&r| r)
}

/// Array.prototype.some() - tests if at least one element passes test
#[dive(swift)]
#[allow(dead_code)]
fn array_some(results: Vec<bool>) -> bool {
    results.iter().any(|&r| r)
}

// ============================================================================
// Conversion Methods
// ============================================================================

/// Array.prototype.join() - joins elements into a string
#[dive(swift)]
#[allow(dead_code)]
fn array_join(arr: JsonValue, separator: Option<String>) -> Result<String, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    let sep = separator.as_deref().unwrap_or(",");

    let parts: Vec<String> = existing
        .iter()
        .map(|v| match v {
            JsonValue::String(s) => s.clone(),
            JsonValue::Number(n) => n.to_string(),
            JsonValue::Bool(b) => b.to_string(),
            JsonValue::Null => "null".to_string(),
            JsonValue::Array(_) => "".to_string(),
            JsonValue::Object(_) => "[object Object]".to_string(),
        })
        .collect();

    Ok(parts.join(sep))
}

/// Array.prototype.toString() - converts array to string
#[dive(swift)]
#[allow(dead_code)]
fn array_to_string(arr: JsonValue) -> Result<String, String> {
    array_join(arr, None)
}

/// Array.prototype.length - returns array length
#[dive(swift)]
#[allow(dead_code)]
fn array_length(arr: JsonValue) -> Result<usize, String> {
    let existing = arr.as_array().ok_or("First argument must be array")?;
    Ok(existing.len())
}

// ============================================================================
// ES2023 Immutable Methods
// ============================================================================

/// Array.prototype.toReversed() - returns new reversed array (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_to_reversed(arr: JsonValue) -> Result<Vec<JsonValue>, String> {
    array_reverse(arr)
}

/// Array.prototype.toSorted() - returns new sorted array (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_to_sorted(arr: JsonValue) -> Result<Vec<JsonValue>, String> {
    array_sort(arr)
}

/// Arguments for toSpliced
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ToSplicedArgs {
    pub arr: JsonValue,
    pub start: i64,
    pub delete_count: Option<usize>,
    pub items: Option<Vec<JsonValue>>,
}

/// Array.prototype.toSpliced() - returns new array with splice applied (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_to_spliced(args: ToSplicedArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;

    let start = if args.start < 0 {
        (len + args.start).max(0) as usize
    } else {
        (args.start as usize).min(arr.len())
    };

    let delete_count = args.delete_count.unwrap_or(arr.len() - start);
    let items = args.items.unwrap_or_default();

    let mut result = Vec::new();
    result.extend_from_slice(&arr[..start]);
    result.extend(items);
    result.extend_from_slice(&arr[(start + delete_count).min(arr.len())..]);

    Ok(result)
}

/// Arguments for with
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WithArgs {
    pub arr: JsonValue,
    pub index: i64,
    pub value: JsonValue,
}

/// Array.prototype.with() - returns new array with element replaced (ES2023)
#[dive(swift)]
#[allow(dead_code)]
fn array_with(args: WithArgs) -> Result<Vec<JsonValue>, String> {
    let arr = args.arr.as_array().ok_or("First argument must be array")?;
    let len = arr.len() as i64;

    let idx = if args.index < 0 {
        len + args.index
    } else {
        args.index
    };

    if idx < 0 || idx >= len {
        return Err("Index out of range".to_string());
    }

    let mut result = arr.clone();
    result[idx as usize] = args.value;
    Ok(result)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Static methods
    #[test]
    fn test_array_is_array() {
        assert!(array_is_array(json!([1, 2, 3])));
        assert!(!array_is_array(json!("not array")));
        assert!(!array_is_array(json!(123)));
    }

    #[test]
    fn test_array_from_array() {
        let result = array_from(json!([1, 2, 3]));
        assert_eq!(result, vec![json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn test_array_from_string() {
        let result = array_from(json!("abc"));
        assert_eq!(result, vec![json!("a"), json!("b"), json!("c")]);
    }

    #[test]
    fn test_array_of() {
        let result = array_of(vec![json!(1), json!(2), json!(3)]);
        assert_eq!(result, vec![json!(1), json!(2), json!(3)]);
    }

    // Mutating methods
    #[test]
    fn test_array_push() {
        let result = array_push(json!([1, 2]), vec![json!(3), json!(4)]).unwrap();
        assert_eq!(result, 4);
    }

    #[test]
    fn test_array_pop() {
        assert_eq!(array_pop(json!([1, 2, 3])).unwrap(), json!(3));
        assert_eq!(array_pop(json!([])).unwrap(), JsonValue::Null);
    }

    #[test]
    fn test_array_shift() {
        assert_eq!(array_shift(json!([1, 2, 3])).unwrap(), json!(1));
        assert_eq!(array_shift(json!([])).unwrap(), JsonValue::Null);
    }

    #[test]
    fn test_array_splice() {
        let args = SpliceArgs {
            arr: json!([1, 2, 3, 4, 5]),
            start: 1,
            delete_count: Some(2),
            items: None,
        };
        let deleted = array_splice(args).unwrap();
        assert_eq!(deleted, vec![json!(2), json!(3)]);
    }

    #[test]
    fn test_array_sort() {
        let result = array_sort(json!([3, 1, 2])).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn test_array_fill() {
        let args = FillArgs {
            arr: json!([1, 2, 3, 4]),
            value: json!(0),
            start: Some(1),
            end: Some(3),
        };
        let result = array_fill(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(0), json!(0), json!(4)]);
    }

    #[test]
    fn test_array_copy_within() {
        let args = CopyWithinArgs {
            arr: json!([1, 2, 3, 4, 5]),
            target: 0,
            start: Some(3),
            end: None,
        };
        let result = array_copy_within(args).unwrap();
        assert_eq!(
            result,
            vec![json!(4), json!(5), json!(3), json!(4), json!(5)]
        );
    }

    // Non-mutating methods
    #[test]
    fn test_array_slice() {
        let args = SliceArgs {
            arr: json!([1, 2, 3, 4, 5]),
            start: Some(1),
            end: Some(4),
        };
        let result = array_slice(args).unwrap();
        assert_eq!(result, vec![json!(2), json!(3), json!(4)]);
    }

    #[test]
    fn test_array_slice_negative() {
        let args = SliceArgs {
            arr: json!([1, 2, 3, 4, 5]),
            start: Some(-3),
            end: Some(-1),
        };
        let result = array_slice(args).unwrap();
        assert_eq!(result, vec![json!(3), json!(4)]);
    }

    #[test]
    fn test_array_concat() {
        let result = array_concat(json!([1, 2]), vec![json!([3, 4])]).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(3), json!(4)]);
    }

    #[test]
    fn test_array_flat() {
        let args = FlatArgs {
            arr: json!([[1, 2], [3, [4, 5]]]),
            depth: Some(1),
        };
        let result = array_flat(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(3), json!([4, 5])]);
    }

    #[test]
    fn test_array_flat_deep() {
        let args = FlatArgs {
            arr: json!([[1, [2, [3]]]]),
            depth: Some(2),
        };
        let result = array_flat(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!([3])]);
    }

    // Search methods
    #[test]
    fn test_array_index_of() {
        assert_eq!(array_index_of(json!([1, 2, 3, 4]), json!(3)).unwrap(), 2);
        assert_eq!(array_index_of(json!([1, 2, 3]), json!(5)).unwrap(), -1);
    }

    #[test]
    fn test_array_last_index_of() {
        assert_eq!(
            array_last_index_of(json!([1, 2, 3, 2, 1]), json!(2)).unwrap(),
            3
        );
        assert_eq!(array_last_index_of(json!([1, 2, 3]), json!(5)).unwrap(), -1);
    }

    #[test]
    fn test_array_includes() {
        assert!(array_includes(json!([1, 2, 3]), json!(2)).unwrap());
        assert!(!array_includes(json!([1, 2, 3]), json!(5)).unwrap());
    }

    #[test]
    fn test_array_at() {
        assert_eq!(array_at(json!([1, 2, 3, 4]), 1).unwrap(), json!(2));
        assert_eq!(array_at(json!([1, 2, 3, 4]), -1).unwrap(), json!(4));
        assert_eq!(array_at(json!([1, 2, 3, 4]), -2).unwrap(), json!(3));
        assert_eq!(array_at(json!([1, 2, 3, 4]), 10).unwrap(), JsonValue::Null);
    }

    #[test]
    fn test_array_find() {
        let args = FindArgs {
            arr: json!([1, 2, 3, 4]),
            results: vec![false, false, true, false],
        };
        assert_eq!(array_find(args).unwrap(), json!(3));
    }

    #[test]
    fn test_array_find_index() {
        let args = FindArgs {
            arr: json!([1, 2, 3, 4]),
            results: vec![false, false, true, false],
        };
        assert_eq!(array_find_index(args).unwrap(), 2);
    }

    #[test]
    fn test_array_find_last() {
        let args = FindArgs {
            arr: json!([1, 2, 3, 4]),
            results: vec![false, true, true, false],
        };
        assert_eq!(array_find_last(args).unwrap(), json!(3));
    }

    #[test]
    fn test_array_find_last_index() {
        let args = FindArgs {
            arr: json!([1, 2, 3, 4]),
            results: vec![false, true, true, false],
        };
        assert_eq!(array_find_last_index(args).unwrap(), 2);
    }

    // Iteration methods
    #[test]
    fn test_array_filter() {
        let args = FindArgs {
            arr: json!([1, 2, 3, 4, 5]),
            results: vec![false, true, false, true, false],
        };
        let result = array_filter(args).unwrap();
        assert_eq!(result, vec![json!(2), json!(4)]);
    }

    #[test]
    fn test_array_every() {
        assert!(array_every(vec![true, true, true]));
        assert!(!array_every(vec![true, false, true]));
    }

    #[test]
    fn test_array_some() {
        assert!(array_some(vec![false, true, false]));
        assert!(!array_some(vec![false, false, false]));
    }

    // Conversion methods
    #[test]
    fn test_array_join() {
        assert_eq!(
            array_join(json!(["a", "b", "c"]), Some("-".to_string())).unwrap(),
            "a-b-c"
        );
        assert_eq!(array_join(json!(["a", "b", "c"]), None).unwrap(), "a,b,c");
    }

    #[test]
    fn test_array_to_string() {
        assert_eq!(array_to_string(json!([1, 2, 3])).unwrap(), "1,2,3");
    }

    // ES2023 methods
    #[test]
    fn test_array_to_reversed() {
        let result = array_to_reversed(json!([1, 2, 3])).unwrap();
        assert_eq!(result, vec![json!(3), json!(2), json!(1)]);
    }

    #[test]
    fn test_array_to_sorted() {
        let result = array_to_sorted(json!([3, 1, 2])).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn test_array_to_spliced() {
        let args = ToSplicedArgs {
            arr: json!([1, 2, 3, 4]),
            start: 1,
            delete_count: Some(2),
            items: Some(vec![json!(5), json!(6)]),
        };
        let result = array_to_spliced(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(5), json!(6), json!(4)]);
    }

    #[test]
    fn test_array_with() {
        let args = WithArgs {
            arr: json!([1, 2, 3, 4]),
            index: 2,
            value: json!(99),
        };
        let result = array_with(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(99), json!(4)]);
    }

    #[test]
    fn test_array_with_negative() {
        let args = WithArgs {
            arr: json!([1, 2, 3, 4]),
            index: -1,
            value: json!(99),
        };
        let result = array_with(args).unwrap();
        assert_eq!(result, vec![json!(1), json!(2), json!(3), json!(99)]);
    }
}
