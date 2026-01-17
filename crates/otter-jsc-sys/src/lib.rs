//! Raw FFI bindings to JavaScriptCore C API
//!
//! This crate provides low-level unsafe bindings to JavaScriptCore.
//! Use the safe wrappers in `otter-runtime` for higher-level access.

#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int, c_uint};

// WTF Timer implementation for bun-webkit integration
pub mod wtf_timer;

// Type aliases for JSC opaque pointers
pub type JSContextGroupRef = *mut c_void;
pub type JSContextRef = *mut c_void;
pub type JSGlobalContextRef = *mut c_void;
pub type JSStringRef = *mut c_void;
pub type JSClassRef = *mut c_void;
pub type JSValueRef = *mut c_void;
pub type JSObjectRef = *mut c_void;
pub type JSPropertyNameArrayRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OtterJscHeapStats {
    pub heap_size: usize,
    pub heap_capacity: usize,
    pub extra_memory: usize,
    pub array_buffer: usize,
}

// Property attributes
pub type JSPropertyAttributes = c_uint;
pub const K_JS_PROPERTY_ATTRIBUTE_NONE: JSPropertyAttributes = 0;
pub const K_JS_PROPERTY_ATTRIBUTE_READ_ONLY: JSPropertyAttributes = 1 << 1;
pub const K_JS_PROPERTY_ATTRIBUTE_DONT_ENUM: JSPropertyAttributes = 1 << 2;
pub const K_JS_PROPERTY_ATTRIBUTE_DONT_DELETE: JSPropertyAttributes = 1 << 3;

// Value types
pub type JSType = c_uint;
pub const K_JS_TYPE_UNDEFINED: JSType = 0;
pub const K_JS_TYPE_NULL: JSType = 1;
pub const K_JS_TYPE_BOOLEAN: JSType = 2;
pub const K_JS_TYPE_NUMBER: JSType = 3;
pub const K_JS_TYPE_STRING: JSType = 4;
pub const K_JS_TYPE_OBJECT: JSType = 5;
pub const K_JS_TYPE_SYMBOL: JSType = 6;

// Callback types
pub type JSObjectCallAsFunctionCallback = Option<
    unsafe extern "C" fn(
        ctx: JSContextRef,
        function: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef,
>;

unsafe extern "C" {
    pub fn otter_jsc_heap_stats(ctx: JSContextRef, out: *mut OtterJscHeapStats) -> bool;
}

// FFI declarations - linking is handled by build.rs
#[cfg(target_os = "macos")]
#[link(name = "JavaScriptCore", kind = "framework")]
unsafe extern "C" {
    // Context functions
    pub fn JSContextGroupCreate() -> JSContextGroupRef;
    pub fn JSContextGroupRelease(group: JSContextGroupRef);

    pub fn JSGlobalContextCreate(global_object_class: JSClassRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextCreateInGroup(
        group: JSContextGroupRef,
        global_object_class: JSClassRef,
    ) -> JSGlobalContextRef;
    pub fn JSGlobalContextRetain(ctx: JSGlobalContextRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    pub fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;

    // String functions
    pub fn JSStringCreateWithUTF8CString(string: *const c_char) -> JSStringRef;
    pub fn JSStringGetLength(string: JSStringRef) -> usize;
    pub fn JSStringGetMaximumUTF8CStringSize(string: JSStringRef) -> usize;
    pub fn JSStringGetUTF8CString(
        string: JSStringRef,
        buffer: *mut c_char,
        buffer_size: usize,
    ) -> usize;
    pub fn JSStringRelease(string: JSStringRef);
    pub fn JSStringIsEqual(a: JSStringRef, b: JSStringRef) -> bool;

    // Value creation
    pub fn JSValueMakeUndefined(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeBoolean(ctx: JSContextRef, boolean: bool) -> JSValueRef;
    pub fn JSValueMakeNumber(ctx: JSContextRef, number: f64) -> JSValueRef;
    pub fn JSValueMakeString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;
    pub fn JSValueMakeFromJSONString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;

    // Value type checking
    pub fn JSValueGetType(ctx: JSContextRef, value: JSValueRef) -> JSType;
    pub fn JSValueIsUndefined(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNull(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNumber(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsString(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsObject(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsArray(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsInstanceOfConstructor(
        ctx: JSContextRef,
        value: JSValueRef,
        constructor: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> bool;

    // Value conversion
    pub fn JSValueToBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueToNumber(ctx: JSContextRef, value: JSValueRef, exception: *mut JSValueRef)
    -> f64;
    pub fn JSValueToStringCopy(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
    pub fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSValueCreateJSONString(
        ctx: JSContextRef,
        value: JSValueRef,
        indent: c_uint,
        exception: *mut JSValueRef,
    ) -> JSStringRef;

    // Value memory management
    pub fn JSValueProtect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSValueUnprotect(ctx: JSContextRef, value: JSValueRef);

    // Object creation
    pub fn JSObjectMake(ctx: JSContextRef, js_class: JSClassRef, data: *mut c_void) -> JSObjectRef;
    pub fn JSObjectMakeFunctionWithCallback(
        ctx: JSContextRef,
        name: JSStringRef,
        callback: JSObjectCallAsFunctionCallback,
    ) -> JSObjectRef;
    pub fn JSObjectMakeArray(
        ctx: JSContextRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool;
    pub fn JSObjectMakeDeferredPromise(
        ctx: JSContextRef,
        resolve: *mut JSObjectRef,
        reject: *mut JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;

    // Object properties
    pub fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        value: JSValueRef,
        attributes: JSPropertyAttributes,
        exception: *mut JSValueRef,
    );
    pub fn JSObjectHasProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
    ) -> bool;
    pub fn JSObjectDeleteProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> bool;
    pub fn JSObjectGetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        value: JSValueRef,
        exception: *mut JSValueRef,
    );

    // Script evaluation
    pub fn JSEvaluateScript(
        ctx: JSContextRef,
        script: JSStringRef,
        this_object: JSObjectRef,
        source_url: JSStringRef,
        starting_line_number: c_int,
        exception: *mut JSValueRef,
    ) -> JSValueRef;

    // Garbage collection
    pub fn JSGarbageCollect(ctx: JSContextRef);
}

// Linux FFI - statically linked bun-webkit
#[cfg(target_os = "linux")]
#[allow(clippy::duplicated_attributes)]
#[link(name = "JavaScriptCore", kind = "static")]
#[link(name = "WTF", kind = "static")]
#[link(name = "bmalloc", kind = "static")]
#[link(name = "icuuc", kind = "static")]
#[link(name = "icui18n", kind = "static")]
#[link(name = "icudata", kind = "static")]
unsafe extern "C" {
    // Context functions
    pub fn JSGlobalContextCreate(global_object_class: JSClassRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRetain(ctx: JSGlobalContextRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    pub fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;

    // String functions
    pub fn JSStringCreateWithUTF8CString(string: *const c_char) -> JSStringRef;
    pub fn JSStringGetLength(string: JSStringRef) -> usize;
    pub fn JSStringGetMaximumUTF8CStringSize(string: JSStringRef) -> usize;
    pub fn JSStringGetUTF8CString(
        string: JSStringRef,
        buffer: *mut c_char,
        buffer_size: usize,
    ) -> usize;
    pub fn JSStringRelease(string: JSStringRef);
    pub fn JSStringIsEqual(a: JSStringRef, b: JSStringRef) -> bool;

    // Value creation
    pub fn JSValueMakeUndefined(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeBoolean(ctx: JSContextRef, boolean: bool) -> JSValueRef;
    pub fn JSValueMakeNumber(ctx: JSContextRef, number: f64) -> JSValueRef;
    pub fn JSValueMakeString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;
    pub fn JSValueMakeFromJSONString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;

    // Value type checking
    pub fn JSValueGetType(ctx: JSContextRef, value: JSValueRef) -> JSType;
    pub fn JSValueIsUndefined(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNull(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNumber(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsString(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsObject(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsArray(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsInstanceOfConstructor(
        ctx: JSContextRef,
        value: JSValueRef,
        constructor: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> bool;

    // Value conversion
    pub fn JSValueToBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueToNumber(ctx: JSContextRef, value: JSValueRef, exception: *mut JSValueRef)
    -> f64;
    pub fn JSValueToStringCopy(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
    pub fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSValueCreateJSONString(
        ctx: JSContextRef,
        value: JSValueRef,
        indent: c_uint,
        exception: *mut JSValueRef,
    ) -> JSStringRef;

    // Value memory management
    pub fn JSValueProtect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSValueUnprotect(ctx: JSContextRef, value: JSValueRef);

    // Object creation
    pub fn JSObjectMake(ctx: JSContextRef, js_class: JSClassRef, data: *mut c_void) -> JSObjectRef;
    pub fn JSObjectMakeFunctionWithCallback(
        ctx: JSContextRef,
        name: JSStringRef,
        callback: JSObjectCallAsFunctionCallback,
    ) -> JSObjectRef;
    pub fn JSObjectMakeArray(
        ctx: JSContextRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool;
    pub fn JSObjectMakeDeferredPromise(
        ctx: JSContextRef,
        resolve: *mut JSObjectRef,
        reject: *mut JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;

    // Object properties
    pub fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        value: JSValueRef,
        attributes: JSPropertyAttributes,
        exception: *mut JSValueRef,
    );
    pub fn JSObjectHasProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
    ) -> bool;
    pub fn JSObjectDeleteProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> bool;
    pub fn JSObjectGetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        value: JSValueRef,
        exception: *mut JSValueRef,
    );

    // Script evaluation
    pub fn JSEvaluateScript(
        ctx: JSContextRef,
        script: JSStringRef,
        this_object: JSObjectRef,
        source_url: JSStringRef,
        starting_line_number: c_int,
        exception: *mut JSValueRef,
    ) -> JSValueRef;

    // Garbage collection
    pub fn JSGarbageCollect(ctx: JSContextRef);
}

// Windows FFI - statically linked bun-webkit
#[cfg(target_os = "windows")]
#[allow(clippy::duplicated_attributes)]
#[link(name = "JavaScriptCore", kind = "static")]
#[link(name = "WTF", kind = "static")]
#[cfg_attr(has_bmalloc, link(name = "bmalloc", kind = "static"))]
unsafe extern "C" {
    // Context functions
    pub fn JSGlobalContextCreate(global_object_class: JSClassRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRetain(ctx: JSGlobalContextRef) -> JSGlobalContextRef;
    pub fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    pub fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;

    // String functions
    pub fn JSStringCreateWithUTF8CString(string: *const c_char) -> JSStringRef;
    pub fn JSStringGetLength(string: JSStringRef) -> usize;
    pub fn JSStringGetMaximumUTF8CStringSize(string: JSStringRef) -> usize;
    pub fn JSStringGetUTF8CString(
        string: JSStringRef,
        buffer: *mut c_char,
        buffer_size: usize,
    ) -> usize;
    pub fn JSStringRelease(string: JSStringRef);
    pub fn JSStringIsEqual(a: JSStringRef, b: JSStringRef) -> bool;

    // Value creation
    pub fn JSValueMakeUndefined(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
    pub fn JSValueMakeBoolean(ctx: JSContextRef, boolean: bool) -> JSValueRef;
    pub fn JSValueMakeNumber(ctx: JSContextRef, number: f64) -> JSValueRef;
    pub fn JSValueMakeString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;
    pub fn JSValueMakeFromJSONString(ctx: JSContextRef, string: JSStringRef) -> JSValueRef;

    // Value type checking
    pub fn JSValueGetType(ctx: JSContextRef, value: JSValueRef) -> JSType;
    pub fn JSValueIsUndefined(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNull(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsNumber(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsString(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsObject(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsArray(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueIsInstanceOfConstructor(
        ctx: JSContextRef,
        value: JSValueRef,
        constructor: JSObjectRef,
        exception: *mut JSValueRef,
    ) -> bool;

    // Value conversion
    pub fn JSValueToBoolean(ctx: JSContextRef, value: JSValueRef) -> bool;
    pub fn JSValueToNumber(ctx: JSContextRef, value: JSValueRef, exception: *mut JSValueRef)
    -> f64;
    pub fn JSValueToStringCopy(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSStringRef;
    pub fn JSValueToObject(
        ctx: JSContextRef,
        value: JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSValueCreateJSONString(
        ctx: JSContextRef,
        value: JSValueRef,
        indent: c_uint,
        exception: *mut JSValueRef,
    ) -> JSStringRef;

    // Value memory management
    pub fn JSValueProtect(ctx: JSContextRef, value: JSValueRef);
    pub fn JSValueUnprotect(ctx: JSContextRef, value: JSValueRef);

    // Object creation
    pub fn JSObjectMake(ctx: JSContextRef, js_class: JSClassRef, data: *mut c_void) -> JSObjectRef;
    pub fn JSObjectMakeFunctionWithCallback(
        ctx: JSContextRef,
        name: JSStringRef,
        callback: JSObjectCallAsFunctionCallback,
    ) -> JSObjectRef;
    pub fn JSObjectMakeArray(
        ctx: JSContextRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;
    pub fn JSObjectCallAsFunction(
        ctx: JSContextRef,
        object: JSObjectRef,
        this_object: JSObjectRef,
        argument_count: usize,
        arguments: *const JSValueRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectIsFunction(ctx: JSContextRef, object: JSObjectRef) -> bool;
    pub fn JSObjectMakeDeferredPromise(
        ctx: JSContextRef,
        resolve: *mut JSObjectRef,
        reject: *mut JSObjectRef,
        exception: *mut JSValueRef,
    ) -> JSObjectRef;

    // Object properties
    pub fn JSObjectGetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        value: JSValueRef,
        attributes: JSPropertyAttributes,
        exception: *mut JSValueRef,
    );
    pub fn JSObjectHasProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
    ) -> bool;
    pub fn JSObjectDeleteProperty(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_name: JSStringRef,
        exception: *mut JSValueRef,
    ) -> bool;
    pub fn JSObjectGetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        exception: *mut JSValueRef,
    ) -> JSValueRef;
    pub fn JSObjectSetPropertyAtIndex(
        ctx: JSContextRef,
        object: JSObjectRef,
        property_index: c_uint,
        value: JSValueRef,
        exception: *mut JSValueRef,
    );

    // Script evaluation
    pub fn JSEvaluateScript(
        ctx: JSContextRef,
        script: JSStringRef,
        this_object: JSObjectRef,
        source_url: JSStringRef,
        starting_line_number: c_int,
        exception: *mut JSValueRef,
    ) -> JSValueRef;

    // Garbage collection
    pub fn JSGarbageCollect(ctx: JSContextRef);
}
