//! Performance API implementation (performance.now, performance.mark, etc.)
//!
//! Provides high-resolution timing and performance measurement APIs
//! compatible with the W3C Performance Timeline specification.

use crate::bindings::*;
use crate::error::JscResult;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::CString;
use std::ptr;
use std::sync::LazyLock;
use std::time::Instant;

/// Global time origin for all contexts
static TIME_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Global start time as Unix timestamp (milliseconds since epoch)
static TIME_ORIGIN_UNIX: LazyLock<f64> = LazyLock::new(|| {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
});

/// Performance entry type
#[derive(Debug, Clone)]
struct PerformanceEntry {
    name: String,
    entry_type: String,
    start_time: f64,
    duration: f64,
}

/// Performance marks and measures storage per context
struct PerformanceState {
    marks: HashMap<String, f64>,
    entries: Vec<PerformanceEntry>,
}

impl Default for PerformanceState {
    fn default() -> Self {
        Self {
            marks: HashMap::new(),
            entries: Vec::new(),
        }
    }
}

/// Global performance state (simplified - in production would be per-context)
static PERFORMANCE_STATE: LazyLock<Mutex<PerformanceState>> =
    LazyLock::new(|| Mutex::new(PerformanceState::default()));

/// Get current high-resolution time in milliseconds since time origin
fn now() -> f64 {
    TIME_ORIGIN.elapsed().as_secs_f64() * 1000.0
}

/// Register the performance API on a context
pub fn register_performance_api(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        // Create performance object
        let perf_obj = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());

        // Register methods
        register_method(ctx, perf_obj, "now", Some(js_performance_now))?;
        register_method(ctx, perf_obj, "mark", Some(js_performance_mark))?;
        register_method(ctx, perf_obj, "measure", Some(js_performance_measure))?;
        register_method(ctx, perf_obj, "getEntries", Some(js_get_entries))?;
        register_method(
            ctx,
            perf_obj,
            "getEntriesByName",
            Some(js_get_entries_by_name),
        )?;
        register_method(
            ctx,
            perf_obj,
            "getEntriesByType",
            Some(js_get_entries_by_type),
        )?;
        register_method(ctx, perf_obj, "clearMarks", Some(js_clear_marks))?;
        register_method(ctx, perf_obj, "clearMeasures", Some(js_clear_measures))?;

        // Set timeOrigin property (read-only)
        let time_origin_name = CString::new("timeOrigin").unwrap();
        let time_origin_ref = JSStringCreateWithUTF8CString(time_origin_name.as_ptr());
        let time_origin_value = JSValueMakeNumber(ctx, *TIME_ORIGIN_UNIX);
        let mut exception: JSValueRef = ptr::null_mut();
        JSObjectSetProperty(
            ctx,
            perf_obj,
            time_origin_ref,
            time_origin_value,
            K_JS_PROPERTY_ATTRIBUTE_READ_ONLY,
            &mut exception,
        );
        JSStringRelease(time_origin_ref);

        // Set performance on globalThis
        let global = JSContextGetGlobalObject(ctx);
        let perf_name = CString::new("performance").unwrap();
        let perf_name_ref = JSStringCreateWithUTF8CString(perf_name.as_ptr());
        JSObjectSetProperty(
            ctx,
            global,
            perf_name_ref,
            perf_obj as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );
        JSStringRelease(perf_name_ref);
    }

    Ok(())
}

unsafe fn register_method(
    ctx: JSContextRef,
    obj: JSObjectRef,
    name: &str,
    callback: JSObjectCallAsFunctionCallback,
) -> JscResult<()> {
    let name_cstr = CString::new(name).unwrap();
    let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
    let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, callback);

    let mut exception: JSValueRef = ptr::null_mut();
    JSObjectSetProperty(
        ctx,
        obj,
        name_ref,
        func as JSValueRef,
        K_JS_PROPERTY_ATTRIBUTE_NONE,
        &mut exception,
    );

    JSStringRelease(name_ref);
    Ok(())
}

/// performance.now() - returns high-resolution timestamp
unsafe extern "C" fn js_performance_now(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    _argument_count: usize,
    _arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    JSValueMakeNumber(ctx, now())
}

/// performance.mark(name) - creates a named timestamp
unsafe extern "C" fn js_performance_mark(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    if argument_count < 1 {
        *exception = crate::apis::make_exception(ctx, "mark() requires a name argument");
        return JSValueMakeUndefined(ctx);
    }

    let name = match crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count) {
        Some(n) => n,
        None => {
            *exception = crate::apis::make_exception(ctx, "mark() name must be a string");
            return JSValueMakeUndefined(ctx);
        }
    };

    let timestamp = now();

    {
        let mut state = PERFORMANCE_STATE.lock();
        state.marks.insert(name.clone(), timestamp);
        state.entries.push(PerformanceEntry {
            name,
            entry_type: "mark".to_string(),
            start_time: timestamp,
            duration: 0.0,
        });
    }

    JSValueMakeUndefined(ctx)
}

/// performance.measure(name, startMark?, endMark?) - creates a measure
unsafe extern "C" fn js_performance_measure(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    if argument_count < 1 {
        *exception = crate::apis::make_exception(ctx, "measure() requires a name argument");
        return JSValueMakeUndefined(ctx);
    }

    let name = match crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count) {
        Some(n) => n,
        None => {
            *exception = crate::apis::make_exception(ctx, "measure() name must be a string");
            return JSValueMakeUndefined(ctx);
        }
    };

    let start_mark = crate::apis::get_arg_as_string(ctx, arguments, 1, argument_count);
    let end_mark = crate::apis::get_arg_as_string(ctx, arguments, 2, argument_count);

    let state = PERFORMANCE_STATE.lock();

    let start_time = match start_mark {
        Some(ref mark) => state.marks.get(mark).copied().unwrap_or(0.0),
        None => 0.0,
    };

    let end_time = match end_mark {
        Some(ref mark) => state.marks.get(mark).copied().unwrap_or_else(now),
        None => now(),
    };

    let duration = end_time - start_time;
    drop(state);

    {
        let mut state = PERFORMANCE_STATE.lock();
        state.entries.push(PerformanceEntry {
            name,
            entry_type: "measure".to_string(),
            start_time,
            duration,
        });
    }

    JSValueMakeUndefined(ctx)
}

/// Helper to convert entries to JS array
unsafe fn entries_to_js_array(ctx: JSContextRef, entries: &[PerformanceEntry]) -> JSValueRef {
    let arr = JSObjectMakeArray(ctx, 0, ptr::null(), ptr::null_mut());

    for (i, entry) in entries.iter().enumerate() {
        let obj = JSObjectMake(ctx, ptr::null_mut(), ptr::null_mut());

        // Set properties
        set_string_property(ctx, obj, "name", &entry.name);
        set_string_property(ctx, obj, "entryType", &entry.entry_type);
        set_number_property(ctx, obj, "startTime", entry.start_time);
        set_number_property(ctx, obj, "duration", entry.duration);

        let mut exc: JSValueRef = ptr::null_mut();
        JSObjectSetPropertyAtIndex(ctx, arr, i as u32, obj as JSValueRef, &mut exc);
    }

    arr as JSValueRef
}

unsafe fn set_string_property(ctx: JSContextRef, obj: JSObjectRef, name: &str, value: &str) {
    let name_cstr = CString::new(name).unwrap();
    let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
    let value_cstr = CString::new(value).unwrap();
    let value_ref = JSStringCreateWithUTF8CString(value_cstr.as_ptr());
    let value_js = JSValueMakeString(ctx, value_ref);

    let mut exc: JSValueRef = ptr::null_mut();
    JSObjectSetProperty(
        ctx,
        obj,
        name_ref,
        value_js,
        K_JS_PROPERTY_ATTRIBUTE_NONE,
        &mut exc,
    );

    JSStringRelease(name_ref);
    JSStringRelease(value_ref);
}

unsafe fn set_number_property(ctx: JSContextRef, obj: JSObjectRef, name: &str, value: f64) {
    let name_cstr = CString::new(name).unwrap();
    let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
    let value_js = JSValueMakeNumber(ctx, value);

    let mut exc: JSValueRef = ptr::null_mut();
    JSObjectSetProperty(
        ctx,
        obj,
        name_ref,
        value_js,
        K_JS_PROPERTY_ATTRIBUTE_NONE,
        &mut exc,
    );

    JSStringRelease(name_ref);
}

/// performance.getEntries() - returns all entries
unsafe extern "C" fn js_get_entries(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    _argument_count: usize,
    _arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let state = PERFORMANCE_STATE.lock();
    entries_to_js_array(ctx, &state.entries)
}

/// performance.getEntriesByName(name) - returns entries by name
unsafe extern "C" fn js_get_entries_by_name(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let name = crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count);

    let state = PERFORMANCE_STATE.lock();
    let filtered: Vec<_> = state
        .entries
        .iter()
        .filter(|e| name.as_ref().map_or(true, |n| &e.name == n))
        .cloned()
        .collect();

    entries_to_js_array(ctx, &filtered)
}

/// performance.getEntriesByType(type) - returns entries by type
unsafe extern "C" fn js_get_entries_by_type(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let entry_type = crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count);

    let state = PERFORMANCE_STATE.lock();
    let filtered: Vec<_> = state
        .entries
        .iter()
        .filter(|e| entry_type.as_ref().map_or(true, |t| &e.entry_type == t))
        .cloned()
        .collect();

    entries_to_js_array(ctx, &filtered)
}

/// performance.clearMarks(name?) - clears marks
unsafe extern "C" fn js_clear_marks(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let name = crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count);

    let mut state = PERFORMANCE_STATE.lock();
    match name {
        Some(n) => {
            state.marks.remove(&n);
            state
                .entries
                .retain(|e| !(e.entry_type == "mark" && e.name == n));
        }
        None => {
            state.marks.clear();
            state.entries.retain(|e| e.entry_type != "mark");
        }
    }

    JSValueMakeUndefined(ctx)
}

/// performance.clearMeasures(name?) - clears measures
unsafe extern "C" fn js_clear_measures(
    ctx: JSContextRef,
    _function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    _exception: *mut JSValueRef,
) -> JSValueRef {
    let name = crate::apis::get_arg_as_string(ctx, arguments, 0, argument_count);

    let mut state = PERFORMANCE_STATE.lock();
    match name {
        Some(n) => {
            state
                .entries
                .retain(|e| !(e.entry_type == "measure" && e.name == n));
        }
        None => {
            state.entries.retain(|e| e.entry_type != "measure");
        }
    }

    JSValueMakeUndefined(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_increases() {
        let t1 = now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = now();
        assert!(t2 > t1);
    }

    #[test]
    fn test_time_origin() {
        let origin = *TIME_ORIGIN_UNIX;
        // Should be a reasonable Unix timestamp (after year 2020)
        assert!(origin > 1577836800000.0); // 2020-01-01
    }
}
