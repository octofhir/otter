//! `otter:ffi` extension — OtterExtension implementation.
//!
//! Registers the FFI module with `dlopen`, `FFIType`, `suffix`, `read.*`,
//! `ptr`, `toArrayBuffer`, `toBuffer`, and `CString`.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use otter_macros::dive;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::globals::to_number;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::capabilities_context;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

use crate::call::{FfiTrampolineData, ffi_jit_trampoline};
use crate::library::FfiLibrary;
use crate::pointer;
use crate::types::{FFIType, FfiSignature};

// ---------------------------------------------------------------------------
// Thread-local NativeContext for JSCallback support
// ---------------------------------------------------------------------------
// When an FFI call is made from JS, we store a raw pointer to the NativeContext
// in this thread-local. If C code invokes a JSCallback during the FFI call,
// the callback reads this to call back into JS.
//
// SAFETY: OtterJS is single-threaded per isolate. The pointer is only valid
// for the duration of the FFI call that set it.
thread_local! {
    static CURRENT_FFI_NCX: Cell<*mut ()> = const { Cell::new(std::ptr::null_mut()) };
}

fn set_ffi_ncx(ncx: &mut NativeContext) {
    CURRENT_FFI_NCX.with(|c| c.set(ncx as *mut NativeContext as *mut ()));
}

fn clear_ffi_ncx() {
    CURRENT_FFI_NCX.with(|c| c.set(std::ptr::null_mut()));
}

/// Get the current NativeContext for use inside JSCallback.
///
/// # Safety
/// Only valid during an FFI call that was initiated from JS.
unsafe fn get_ffi_ncx<'a>() -> Option<&'a mut NativeContext<'a>> {
    let ptr = CURRENT_FFI_NCX.with(|c| c.get());
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *(ptr as *mut NativeContext<'a>) })
    }
}

/// The `otter:ffi` extension.
pub struct OtterFfiExtension;

impl OtterExtension for OtterFfiExtension {
    fn name(&self) -> &str {
        "otter_ffi"
    }

    fn profiles(&self) -> &[Profile] {
        static PROFILES: [Profile; 1] = [Profile::Full];
        &PROFILES
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static SPECIFIERS: [&str; 1] = ["otter:ffi"];
        &SPECIFIERS
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let mut ns = ctx.module_namespace();

        // dlopen(path, symbols)
        let (name, f, len) = ffi_dlopen_decl();
        ns = ns.function(name, f, len);

        // suffix — platform-specific shared library extension
        ns = ns.property(
            "suffix",
            Value::string(JsString::intern(pointer::platform_suffix())),
        );

        // ptr(typedArrayOrBuffer) — extract raw pointer
        let (name, f, len) = ffi_ptr_decl();
        ns = ns.function(name, f, len);

        // toArrayBuffer(ptr, byteOffset?, byteLength?)
        let (name, f, len) = ffi_to_array_buffer_decl();
        ns = ns.function(name, f, len);

        // toBuffer(ptr, byteOffset?, byteLength?)
        let (name, f, len) = ffi_to_buffer_decl();
        ns = ns.function(name, f, len);

        // CFunction(definition) — create callable from raw function pointer
        let (name, f, len) = ffi_cfunction_decl();
        ns = ns.function(name, f, len);

        // linkSymbols(symbols) — bind multiple function pointers
        let (name, f, len) = ffi_link_symbols_decl();
        ns = ns.function(name, f, len);

        // JSCallback(callback, definition)
        let (name, f, len) = ffi_js_callback_decl();
        ns = ns.function(name, f, len);

        // CString(ptr, byteOffset?, byteLength?)
        let (name, f, len) = ffi_cstring_ctor_decl();
        ns = ns.function(name, f, len);

        // read.* — direct memory reads
        let read_obj = build_read_namespace(ctx);
        ns = ns.property("read", Value::object(read_obj));

        // FFIType — type enum object
        let ffi_type_obj = build_ffi_type_enum(ctx);
        ns = ns.property("FFIType", Value::object(ffi_type_obj));

        Some(ns.build())
    }
}

/// Build the `read` namespace object with direct memory read functions.
fn build_read_namespace(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let mut ns = ctx.module_namespace();
    let read_fns: &[fn() -> (
        &'static str,
        Arc<dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync>,
        u32,
    )] = &[
        ffi_read_u8_decl,
        ffi_read_i8_decl,
        ffi_read_u16_decl,
        ffi_read_i16_decl,
        ffi_read_u32_decl,
        ffi_read_i32_decl,
        ffi_read_u64_decl,
        ffi_read_i64_decl,
        ffi_read_f32_decl,
        ffi_read_f64_decl,
        ffi_read_ptr_decl,
        ffi_read_intptr_decl,
    ];
    for decl in read_fns {
        let (name, f, len) = decl();
        ns = ns.function(name, f, len);
    }
    ns.build()
}

/// Build the `FFIType` enum object (e.g. FFIType.i32 === 5).
fn build_ffi_type_enum(ctx: &mut RegistrationContext) -> GcRef<JsObject> {
    let obj = ctx.new_object();
    let types = [
        FFIType::Char,
        FFIType::I8,
        FFIType::U8,
        FFIType::I16,
        FFIType::U16,
        FFIType::I32,
        FFIType::U32,
        FFIType::I64,
        FFIType::U64,
        FFIType::F64,
        FFIType::F32,
        FFIType::Bool,
        FFIType::Ptr,
        FFIType::Void,
        FFIType::CString,
        FFIType::I64Fast,
        FFIType::U64Fast,
        FFIType::Function,
    ];
    for ty in types {
        let _ = obj.set(
            PropertyKey::string(ty.name()),
            Value::number(ty as u8 as f64),
        );
    }
    obj
}

// ---------------------------------------------------------------------------
// Permission check helper
// ---------------------------------------------------------------------------

fn require_ffi() -> Result<(), VmError> {
    if !capabilities_context::can_ffi() {
        return Err(VmError::type_error(
            "PermissionDenied: FFI access denied. Use --allow-ffi to grant FFI permissions",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helper: parse FFI type from JS value (string name or integer)
// ---------------------------------------------------------------------------

fn parse_ffi_type_value(val: &Value) -> Result<FFIType, VmError> {
    if let Some(s) = val.as_string() {
        FFIType::from_name(s.as_str())
            .ok_or_else(|| VmError::type_error(format!("Invalid FFI type: '{}'", s.as_str())))
    } else if let Some(n) = val.as_number() {
        FFIType::from_u8(n as u8)
            .ok_or_else(|| VmError::type_error(format!("Invalid FFI type number: {}", n)))
    } else {
        Err(VmError::type_error("FFI type must be a string or number"))
    }
}

/// Parse an args array from JS (e.g. `["i32", "ptr"]`) into Vec<FFIType>.
fn parse_ffi_args(val: &Value) -> Result<Vec<FFIType>, VmError> {
    if val.is_undefined() || val.is_null() {
        return Ok(Vec::new());
    }
    let obj = val
        .as_object()
        .ok_or_else(|| VmError::type_error("FFI args must be an array"))?;
    let len = obj
        .get(&PropertyKey::string("length"))
        .map(|v| to_number(&v) as usize)
        .unwrap_or(0);

    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let elem = obj
            .get(&PropertyKey::index(i as u32))
            .unwrap_or(Value::undefined());
        result.push(parse_ffi_type_value(&elem)?);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// dlopen(path, symbolDeclarations)
// ---------------------------------------------------------------------------

#[dive(name = "dlopen", length = 2)]
fn ffi_dlopen(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;

    let path = args.first().and_then(|v| v.as_string()).ok_or_else(|| {
        VmError::type_error("dlopen: first argument must be a string (library path)")
    })?;
    let path_str = path.as_str();

    let symbols_obj = args.get(1).and_then(|v| v.as_object()).ok_or_else(|| {
        VmError::type_error("dlopen: second argument must be an object (symbol declarations)")
    })?;

    // Parse symbol declarations
    let mut signatures = HashMap::new();
    let keys = symbols_obj.own_keys();
    for key in &keys {
        let key_str = match key {
            PropertyKey::String(s) => s.as_str().to_string(),
            PropertyKey::Index(i) => i.to_string(),
            _ => continue,
        };
        let decl = symbols_obj.get(key).unwrap_or(Value::undefined());
        let decl_obj = decl.as_object().ok_or_else(|| {
            VmError::type_error(format!(
                "Symbol '{}' declaration must be an object",
                key_str
            ))
        })?;

        let args_val = decl_obj
            .get(&PropertyKey::string("args"))
            .unwrap_or(Value::undefined());
        let returns_val = decl_obj
            .get(&PropertyKey::string("returns"))
            .unwrap_or(Value::undefined());

        let arg_types = parse_ffi_args(&args_val)?;
        let return_type = if returns_val.is_undefined() {
            FFIType::Void
        } else {
            parse_ffi_type_value(&returns_val)?
        };

        signatures.insert(
            key_str,
            FfiSignature {
                args: arg_types,
                returns: return_type,
            },
        );
    }

    // Open the library
    let lib =
        FfiLibrary::open(path_str, &signatures).map_err(|e| VmError::type_error(e.to_string()))?;

    // Create the library JS object
    let lib_obj = create_library_object(lib, ncx)?;
    Ok(Value::object(lib_obj))
}

/// Wrap an FfiLibrary in a JS object with `.symbols` and `.close()`.
fn create_library_object(
    lib: FfiLibrary,
    ncx: &mut NativeContext,
) -> Result<GcRef<JsObject>, VmError> {
    let lib_rc = Arc::new(Mutex::new(Some(lib)));

    // Build .symbols namespace
    let symbols_obj = GcRef::new(JsObject::new(Value::null()));

    // For each bound symbol, create a callable JS function
    {
        let lib_ref = lib_rc.lock().unwrap();
        let lib_inner = lib_ref.as_ref().unwrap();
        let names: Vec<String> = lib_inner.symbol_names().map(String::from).collect();

        for sym_name in names {
            let lib_clone = Arc::clone(&lib_rc);
            let name_clone = sym_name.clone();

            // Get the signature for this symbol
            let sig = lib_inner.symbol(sym_name.as_str()).unwrap();
            let arg_types = sig.signature.args.clone();
            let arg_count = arg_types.len();
            let return_type = sig.signature.returns;

            let native_fn: Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            > = Arc::new(move |_this, args, ncx| {
                let lib_guard = lib_clone.lock().unwrap();
                let lib_inner = lib_guard
                    .as_ref()
                    .ok_or_else(|| VmError::type_error("FFI library has been closed"))?;

                // Marshal JS values to raw u64 args
                let mut raw_args = Vec::with_capacity(arg_types.len());
                for (i, ty) in arg_types.iter().enumerate() {
                    let val = args.get(i).copied().unwrap_or(Value::undefined());
                    let raw = marshal_value_to_raw(val, *ty)?;
                    raw_args.push(raw);
                }

                // Set thread-local NativeContext for JSCallback support
                set_ffi_ncx(ncx);
                let raw_result = lib_inner.call_raw(&name_clone, &raw_args);
                clear_ffi_ncx();

                let raw_result = raw_result.map_err(|e| VmError::type_error(e.to_string()))?;

                marshal_raw_to_value(raw_result, return_type)
            });

            let fn_obj = GcRef::new(JsObject::new(Value::null()));
            fn_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(arg_count as f64)),
            );
            fn_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern(&sym_name))),
            );

            let fn_val = Value::native_function_with_proto_and_object(
                native_fn,
                ncx.memory_manager().clone(),
                ncx.global()
                    .get(&PropertyKey::string("Function"))
                    .and_then(|v| v.as_object())
                    .and_then(|c| c.get(&PropertyKey::string("prototype")))
                    .and_then(|v| v.as_object())
                    .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null()))),
                fn_obj,
            );

            // Set FFI call info for JIT fast path
            let fn_ptr_raw = sig.ptr as usize;
            let trampoline_data = Box::new(FfiTrampolineData::new(&sig.signature));
            let opaque = Box::into_raw(trampoline_data) as *const ();
            unsafe {
                fn_val.set_ffi_call_info(otter_vm_core::value::FfiCallInfo {
                    trampoline: ffi_jit_trampoline,
                    fn_ptr: fn_ptr_raw,
                    opaque,
                    opaque_drop: Some(|p| {
                        drop(Box::from_raw(p as *mut FfiTrampolineData));
                    }),
                    arg_count: arg_count as u16,
                });
            }

            let _ = symbols_obj.set(PropertyKey::string(&sym_name), fn_val);
        }
    }

    // Build library object
    let lib_obj = GcRef::new(JsObject::new(Value::null()));
    let _ = lib_obj.set(PropertyKey::string("symbols"), Value::object(symbols_obj));

    // .close() method
    let lib_close = Arc::clone(&lib_rc);
    let close_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, _args, _ncx| {
        let mut lib_guard = lib_close.lock().unwrap();
        *lib_guard = None; // Drop the library
        Ok(Value::undefined())
    });
    let close_fn_obj = GcRef::new(JsObject::new(Value::null()));
    let close_val = Value::native_function_with_proto_and_object(
        close_fn,
        ncx.memory_manager().clone(),
        ncx.global()
            .get(&PropertyKey::string("Function"))
            .and_then(|v| v.as_object())
            .and_then(|c| c.get(&PropertyKey::string("prototype")))
            .and_then(|v| v.as_object())
            .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null()))),
        close_fn_obj,
    );
    let _ = lib_obj.set(PropertyKey::string("close"), close_val);

    Ok(lib_obj)
}

// ---------------------------------------------------------------------------
// Value marshaling: JS Value <-> raw u64
// ---------------------------------------------------------------------------

fn marshal_value_to_raw(val: Value, ty: FFIType) -> Result<u64, VmError> {
    match ty {
        FFIType::I8 | FFIType::Char => {
            let n = to_number(&val) as i8;
            Ok(n as u64)
        }
        FFIType::U8 => {
            let n = to_number(&val) as u8;
            Ok(n as u64)
        }
        FFIType::Bool => {
            let b = val.to_boolean() as u8;
            Ok(b as u64)
        }
        FFIType::I16 => {
            let n = to_number(&val) as i16;
            Ok(n as u64)
        }
        FFIType::U16 => {
            let n = to_number(&val) as u16;
            Ok(n as u64)
        }
        FFIType::I32 => {
            let n = to_number(&val) as i32;
            Ok(n as u64)
        }
        FFIType::U32 => {
            let n = to_number(&val) as u32;
            Ok(n as u64)
        }
        FFIType::I64 | FFIType::I64Fast => {
            let n = to_number(&val) as i64;
            Ok(n as u64)
        }
        FFIType::U64 | FFIType::U64Fast => {
            let n = to_number(&val) as u64;
            Ok(n)
        }
        FFIType::F32 => {
            let n = to_number(&val) as f32;
            Ok(n.to_bits() as u64)
        }
        FFIType::F64 => {
            let n = to_number(&val);
            Ok(n.to_bits())
        }
        FFIType::Ptr | FFIType::Function => {
            if val.is_null() || val.is_undefined() {
                Ok(0)
            } else {
                let n = to_number(&val) as u64;
                Ok(n)
            }
        }
        FFIType::CString => {
            // CString: accept a string value and convert to a C pointer.
            // For safety, we create a temporary CString and leak it.
            // The caller is responsible for managing the lifetime.
            if val.is_null() || val.is_undefined() {
                Ok(0)
            } else if let Some(s) = val.as_string() {
                let cstr = std::ffi::CString::new(s.as_str())
                    .map_err(|_| VmError::type_error("String contains null byte"))?;
                let ptr = cstr.into_raw() as u64;
                Ok(ptr)
            } else {
                let n = to_number(&val) as u64;
                Ok(n)
            }
        }
        FFIType::Void => Ok(0),
    }
}

fn marshal_raw_to_value(raw: u64, ty: FFIType) -> Result<Value, VmError> {
    match ty {
        FFIType::Void => Ok(Value::undefined()),
        FFIType::Bool => Ok(Value::boolean(raw != 0)),
        FFIType::I8 | FFIType::Char => {
            let n = raw as i8;
            Ok(Value::number(n as f64))
        }
        FFIType::U8 => {
            let n = raw as u8;
            Ok(Value::number(n as f64))
        }
        FFIType::I16 => {
            let n = raw as i16;
            Ok(Value::number(n as f64))
        }
        FFIType::U16 => {
            let n = raw as u16;
            Ok(Value::number(n as f64))
        }
        FFIType::I32 => {
            let n = raw as i32;
            Ok(Value::number(n as f64))
        }
        FFIType::U32 => {
            let n = raw as u32;
            Ok(Value::number(n as f64))
        }
        FFIType::I64 | FFIType::I64Fast => {
            let n = raw as i64;
            Ok(Value::number(n as f64))
        }
        FFIType::U64 | FFIType::U64Fast => Ok(Value::number(raw as f64)),
        FFIType::F32 => {
            let f = f32::from_bits(raw as u32);
            Ok(Value::number(f as f64))
        }
        FFIType::F64 => {
            let f = f64::from_bits(raw);
            Ok(Value::number(f))
        }
        FFIType::Ptr | FFIType::Function => {
            if raw == 0 {
                Ok(Value::null())
            } else {
                Ok(Value::number(raw as f64))
            }
        }
        FFIType::CString => {
            if raw == 0 {
                Ok(Value::null())
            } else {
                let s = unsafe { pointer::read_cstring(raw as usize, 0) }
                    .map_err(|e| VmError::type_error(e.to_string()))?;
                Ok(Value::string(JsString::intern(&s)))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ptr(typedArrayOrBuffer) — extract raw pointer as number
// ---------------------------------------------------------------------------

#[dive(name = "ptr", length = 1)]
fn ffi_ptr(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;
    let val = args.first().copied().unwrap_or(Value::undefined());

    if val.is_null() || val.is_undefined() {
        return Ok(Value::null());
    }

    let byte_offset = args.get(1).map(|v| to_number(v) as usize).unwrap_or(0);

    // Try direct TypedArray value (TAG_PTR_OTHER)
    let ta_opt = val.as_typed_array().or_else(|| {
        // TypedArrays created by constructors are stored as plain objects
        // with an internal __TypedArrayData__ property
        val.as_object()
            .and_then(|obj| obj.get(&PropertyKey::string("__TypedArrayData__")))
            .and_then(|v| v.as_typed_array())
    });

    if let Some(ta) = ta_opt {
        if ta.is_detached() {
            return Err(VmError::type_error("ptr(): TypedArray buffer is detached"));
        }
        let buf = ta.buffer();
        let base = buf.data_ptr();
        if base == 0 {
            return Ok(Value::null());
        }
        let addr = base + ta.byte_offset() + byte_offset;
        return Ok(Value::number(addr as f64));
    }

    // ArrayBuffer — get data_ptr directly
    if let Some(ab) = val.as_array_buffer() {
        if ab.is_detached() {
            return Err(VmError::type_error("ptr(): ArrayBuffer is detached"));
        }
        let base = ab.data_ptr();
        if base == 0 {
            return Ok(Value::null());
        }
        let addr = base + byte_offset;
        return Ok(Value::number(addr as f64));
    }

    Err(VmError::type_error(
        "ptr(): argument must be a TypedArray or ArrayBuffer",
    ))
}

// ---------------------------------------------------------------------------
// toArrayBuffer(ptr, byteOffset?, byteLength?) — zero-copy view from pointer
// ---------------------------------------------------------------------------

#[dive(name = "toArrayBuffer", length = 1)]
fn ffi_to_array_buffer(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;
    let raw_ptr = args.first().map(|v| to_number(v) as usize).unwrap_or(0);
    if raw_ptr == 0 {
        return Err(VmError::type_error("toArrayBuffer(): null pointer"));
    }

    let byte_offset = args.get(1).map(|v| to_number(v) as usize).unwrap_or(0);
    let addr = raw_ptr + byte_offset;

    // byteLength is required for toArrayBuffer (no null-terminator scanning)
    let byte_length = args
        .get(2)
        .map(|v| to_number(v) as usize)
        .ok_or_else(|| VmError::type_error("toArrayBuffer(): byteLength is required"))?;

    // Copy the data from the raw pointer into a new ArrayBuffer
    let data = unsafe { std::slice::from_raw_parts(addr as *const u8, byte_length) };
    let ab_proto = ncx
        .global()
        .get(&PropertyKey::string("ArrayBuffer"))
        .and_then(|v| v.as_object())
        .and_then(|c| c.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null())));
    let ab = GcRef::new(otter_vm_core::array_buffer::JsArrayBuffer::from_data(
        data.to_vec(),
        ab_proto,
    ));
    Ok(Value::array_buffer(ab))
}

// ---------------------------------------------------------------------------
// toBuffer(ptr, byteOffset?, byteLength?) — same as toArrayBuffer for now
// ---------------------------------------------------------------------------

#[dive(name = "toBuffer", length = 1)]
fn ffi_to_buffer(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    // For now, toBuffer behaves the same as toArrayBuffer.
    // A full Buffer class (Node.js compat) would wrap this differently.
    ffi_to_array_buffer(args, ncx)
}

// ---------------------------------------------------------------------------
// CFunction(definition) — create callable from raw function pointer
// ---------------------------------------------------------------------------

#[dive(name = "CFunction", length = 1)]
fn ffi_cfunction(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;
    let def = args.first().and_then(|v| v.as_object()).ok_or_else(|| {
        VmError::type_error("CFunction: argument must be an object with ptr, args, returns")
    })?;

    let ptr_val = def
        .get(&PropertyKey::string("ptr"))
        .ok_or_else(|| VmError::type_error("CFunction: definition must have a ptr field"))?;
    let fn_ptr = to_number(&ptr_val) as usize;
    if fn_ptr == 0 {
        return Err(VmError::type_error("CFunction: ptr must be non-null"));
    }

    let args_val = def
        .get(&PropertyKey::string("args"))
        .unwrap_or(Value::undefined());
    let returns_val = def
        .get(&PropertyKey::string("returns"))
        .unwrap_or(Value::undefined());

    let arg_types = parse_ffi_args(&args_val)?;
    let arg_count = arg_types.len();
    let return_type = if returns_val.is_undefined() {
        FFIType::Void
    } else {
        parse_ffi_type_value(&returns_val)?
    };

    let sig = FfiSignature {
        args: arg_types.clone(),
        returns: return_type,
    };

    let native_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, call_args, ncx| {
        let mut raw_args = Vec::with_capacity(arg_types.len());
        for (i, ty) in arg_types.iter().enumerate() {
            let val = call_args.get(i).copied().unwrap_or(Value::undefined());
            raw_args.push(marshal_value_to_raw(val, *ty)?);
        }
        set_ffi_ncx(ncx);
        let raw_result = unsafe { crate::call::ffi_call(fn_ptr as *const (), &sig, &raw_args) };
        clear_ffi_ncx();
        let raw_result = raw_result.map_err(|e| VmError::type_error(e.to_string()))?;
        marshal_raw_to_value(raw_result, return_type)
    });

    let fn_obj = GcRef::new(JsObject::new(Value::null()));
    fn_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(arg_count as f64)),
    );

    let fn_proto = ncx
        .global()
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|c| c.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null())));

    let fn_val = Value::native_function_with_proto_and_object(
        native_fn,
        ncx.memory_manager().clone(),
        fn_proto,
        fn_obj,
    );

    Ok(fn_val)
}

// ---------------------------------------------------------------------------
// linkSymbols(symbols) — bind multiple function pointers at once
// ---------------------------------------------------------------------------

#[dive(name = "linkSymbols", length = 1)]
fn ffi_link_symbols(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;
    let symbols_decl = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("linkSymbols: argument must be an object"))?;

    let symbols_obj = GcRef::new(JsObject::new(Value::null()));
    let keys = symbols_decl.own_keys();

    let fn_proto = ncx
        .global()
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|c| c.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null())));

    for key in &keys {
        let key_str = match key {
            PropertyKey::String(s) => s.as_str().to_string(),
            PropertyKey::Index(i) => i.to_string(),
            _ => continue,
        };

        let decl = symbols_decl.get(key).unwrap_or(Value::undefined());
        let decl_obj = decl.as_object().ok_or_else(|| {
            VmError::type_error(format!(
                "linkSymbols: '{}' declaration must be an object",
                key_str
            ))
        })?;

        let ptr_val = decl_obj.get(&PropertyKey::string("ptr")).ok_or_else(|| {
            VmError::type_error(format!("linkSymbols: '{}' must have a ptr field", key_str))
        })?;
        let fn_ptr = to_number(&ptr_val) as usize;
        if fn_ptr == 0 {
            return Err(VmError::type_error(format!(
                "linkSymbols: '{}' ptr must be non-null",
                key_str
            )));
        }

        let args_val = decl_obj
            .get(&PropertyKey::string("args"))
            .unwrap_or(Value::undefined());
        let returns_val = decl_obj
            .get(&PropertyKey::string("returns"))
            .unwrap_or(Value::undefined());

        let arg_types = parse_ffi_args(&args_val)?;
        let arg_count = arg_types.len();
        let return_type = if returns_val.is_undefined() {
            FFIType::Void
        } else {
            parse_ffi_type_value(&returns_val)?
        };

        let sig = FfiSignature {
            args: arg_types.clone(),
            returns: return_type,
        };

        let native_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(move |_this, call_args, ncx| {
            let mut raw_args = Vec::with_capacity(arg_types.len());
            for (i, ty) in arg_types.iter().enumerate() {
                let val = call_args.get(i).copied().unwrap_or(Value::undefined());
                raw_args.push(marshal_value_to_raw(val, *ty)?);
            }
            set_ffi_ncx(ncx);
            let raw_result = unsafe { crate::call::ffi_call(fn_ptr as *const (), &sig, &raw_args) };
            clear_ffi_ncx();
            let raw_result = raw_result.map_err(|e| VmError::type_error(e.to_string()))?;
            marshal_raw_to_value(raw_result, return_type)
        });

        let fn_obj = GcRef::new(JsObject::new(Value::null()));
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(arg_count as f64)),
        );
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(&key_str))),
        );

        let fn_val = Value::native_function_with_proto_and_object(
            native_fn,
            ncx.memory_manager().clone(),
            fn_proto,
            fn_obj,
        );

        let _ = symbols_obj.set(PropertyKey::string(&key_str), fn_val);
    }

    // Build library-like object with .symbols and .close()
    let lib_obj = GcRef::new(JsObject::new(Value::null()));
    let _ = lib_obj.set(PropertyKey::string("symbols"), Value::object(symbols_obj));

    // .close() is a no-op for linkSymbols (no library to unload)
    let close_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let close_fn_obj = GcRef::new(JsObject::new(Value::null()));
    let close_val = Value::native_function_with_proto_and_object(
        close_fn,
        ncx.memory_manager().clone(),
        fn_proto,
        close_fn_obj,
    );
    let _ = lib_obj.set(PropertyKey::string("close"), close_val);

    Ok(Value::object(lib_obj))
}

// ---------------------------------------------------------------------------
// read.* functions — direct memory access
// ---------------------------------------------------------------------------

macro_rules! read_fn {
    ($fn_name:ident, $dive_name:expr, $read_fn:path, $result_type:ty) => {
        #[dive(name = $dive_name, length = 2)]
        fn $fn_name(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
            require_ffi()?;
            let ptr = args.first().map(|v| to_number(v) as usize).unwrap_or(0);
            let offset = args.get(1).map(|v| to_number(v) as usize).unwrap_or(0);
            let val =
                unsafe { $read_fn(ptr, offset) }.map_err(|e| VmError::type_error(e.to_string()))?;
            Ok(Value::number(val as f64))
        }
    };
}

read_fn!(ffi_read_u8, "u8", pointer::read_u8, u8);
read_fn!(ffi_read_i8, "i8", pointer::read_i8, i8);
read_fn!(ffi_read_u16, "u16", pointer::read_u16, u16);
read_fn!(ffi_read_i16, "i16", pointer::read_i16, i16);
read_fn!(ffi_read_u32, "u32", pointer::read_u32, u32);
read_fn!(ffi_read_i32, "i32", pointer::read_i32, i32);
read_fn!(ffi_read_u64, "u64", pointer::read_u64, u64);
read_fn!(ffi_read_i64, "i64", pointer::read_i64, i64);
read_fn!(ffi_read_f32, "f32", pointer::read_f32, f32);
read_fn!(ffi_read_f64, "f64", pointer::read_f64, f64);
read_fn!(ffi_read_ptr, "ptr", pointer::read_ptr, usize);
read_fn!(ffi_read_intptr, "intptr", pointer::read_intptr, isize);

// ---------------------------------------------------------------------------
// JSCallback — wrap a JS function as a C-callable function pointer
// ---------------------------------------------------------------------------

/// Persistent data for a JSCallback closure.
/// Boxed and leaked so the libffi closure can reference it with 'static lifetime.
struct JsCallbackData {
    /// Raw bits of the JS function Value (Value is Copy/u64)
    js_func_bits: u64,
    arg_types: Vec<FFIType>,
    return_type: FFIType,
}

// SAFETY: JsCallbackData only references JS values by raw bits (u64).
// It is only accessed from the single VM thread.
unsafe impl Send for JsCallbackData {}
unsafe impl Sync for JsCallbackData {}

/// The C-compatible callback invoked by libffi when the closure is called.
///
/// # Safety
/// - `args` must point to valid argument pointers matching the CIF.
/// - `userdata` must point to a valid `JsCallbackData`.
/// - A valid NativeContext must be available via thread-local `CURRENT_FFI_NCX`.
unsafe extern "C" fn js_callback_trampoline(
    _cif: &libffi::low::ffi_cif,
    result: &mut u64,
    args: *const *const std::ffi::c_void,
    userdata: &JsCallbackData,
) {
    // Get the NativeContext from thread-local
    let ncx = match unsafe { get_ffi_ncx() } {
        Some(ncx) => ncx,
        None => {
            // No NativeContext available — callback invoked outside an FFI call.
            // Return 0/undefined and hope for the best.
            *result = 0;
            return;
        }
    };

    // Reconstruct the JS function from raw bits
    let js_func = unsafe { Value::from_bits_raw(userdata.js_func_bits) };

    // Unmarshal C arguments to JS values
    let mut js_args = Vec::with_capacity(userdata.arg_types.len());
    for (i, ty) in userdata.arg_types.iter().enumerate() {
        let arg_ptr = unsafe { *args.add(i) };
        let raw: u64 = match ty {
            FFIType::I8 | FFIType::Char => unsafe { *(arg_ptr as *const i8) as u64 },
            FFIType::U8 | FFIType::Bool => unsafe { *(arg_ptr as *const u8) as u64 },
            FFIType::I16 => unsafe { *(arg_ptr as *const i16) as u64 },
            FFIType::U16 => unsafe { *(arg_ptr as *const u16) as u64 },
            FFIType::I32 => unsafe { *(arg_ptr as *const i32) as u64 },
            FFIType::U32 => unsafe { *(arg_ptr as *const u32) as u64 },
            FFIType::I64 | FFIType::I64Fast => unsafe { *(arg_ptr as *const i64) as u64 },
            FFIType::U64 | FFIType::U64Fast => unsafe { *(arg_ptr as *const u64) },
            FFIType::F32 => {
                let f = unsafe { *(arg_ptr as *const f32) };
                f.to_bits() as u64
            }
            FFIType::F64 => {
                let f = unsafe { *(arg_ptr as *const f64) };
                f.to_bits()
            }
            FFIType::Ptr | FFIType::Function | FFIType::CString => unsafe {
                *(arg_ptr as *const usize) as u64
            },
            FFIType::Void => 0,
        };
        match marshal_raw_to_value(raw, *ty) {
            Ok(v) => js_args.push(v),
            Err(_) => js_args.push(Value::undefined()),
        }
    }

    // Call the JS function
    match ncx.call_function(&js_func, Value::undefined(), &js_args) {
        Ok(ret_val) => {
            // Marshal the JS return value back to C
            match marshal_value_to_raw(ret_val, userdata.return_type) {
                Ok(raw) => *result = raw,
                Err(_) => *result = 0,
            }
        }
        Err(_) => {
            // JS threw an exception — return 0
            *result = 0;
        }
    }
}

/// Wraps a libffi closure and its leaked userdata for cleanup.
struct JsCallbackClosure {
    /// The libffi closure (owns the ffi_closure allocation).
    /// We use a raw pointer because Closure<'a> has a lifetime we need to erase.
    _closure_alloc: *mut libffi::low::ffi_closure,
    _closure_code: libffi::low::CodePtr,
    _cif: Box<libffi::middle::Cif>,
    /// Leaked Box pointer for cleanup
    userdata_ptr: *mut JsCallbackData,
    /// The callable code pointer address (kept for potential future use in JIT path)
    _code_ptr: usize,
}

impl Drop for JsCallbackClosure {
    fn drop(&mut self) {
        unsafe {
            libffi::low::closure_free(self._closure_alloc);
            // Reclaim the leaked userdata
            let _ = Box::from_raw(self.userdata_ptr);
        }
    }
}

// SAFETY: JsCallbackClosure is only used from the VM thread
unsafe impl Send for JsCallbackClosure {}
unsafe impl Sync for JsCallbackClosure {}

/// Create a JSCallback: wraps a JS function as a C-callable function pointer.
///
/// Usage from JS:
/// ```js
/// const cb = new JSCallback(
///   (x) => x * 2,
///   { args: [FFIType.i32], returns: FFIType.i32 },
/// );
/// nativeFunc(cb.ptr);
/// cb.close();
/// ```
#[dive(name = "JSCallback", length = 2)]
fn ffi_js_callback(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;

    let js_func = args.first().copied().unwrap_or(Value::undefined());
    if js_func.as_function().is_none() && js_func.as_object().is_none() {
        return Err(VmError::type_error(
            "JSCallback: first argument must be a function",
        ));
    }

    let def = args.get(1).and_then(|v| v.as_object()).ok_or_else(|| {
        VmError::type_error("JSCallback: second argument must be { args, returns }")
    })?;

    let args_val = def
        .get(&PropertyKey::string("args"))
        .unwrap_or(Value::undefined());
    let returns_val = def
        .get(&PropertyKey::string("returns"))
        .unwrap_or(Value::undefined());

    let arg_types = parse_ffi_args(&args_val)?;
    let return_type = if returns_val.is_undefined() {
        FFIType::Void
    } else {
        parse_ffi_type_value(&returns_val)?
    };

    // Build the libffi CIF
    let libffi_arg_types: Vec<_> = arg_types.iter().map(|t| t.to_libffi_type()).collect();
    let libffi_ret_type = return_type.to_libffi_type();
    let cif = Box::new(libffi::middle::Cif::new(libffi_arg_types, libffi_ret_type));

    // Leak the userdata so it has 'static lifetime
    let userdata = Box::new(JsCallbackData {
        js_func_bits: js_func.to_bits_raw(),
        arg_types,
        return_type,
    });
    let userdata_ptr = Box::into_raw(userdata);

    // Allocate and prepare the libffi closure
    let (alloc, code) = libffi::low::closure_alloc();
    assert!(!alloc.is_null(), "libffi closure_alloc returned null");

    unsafe {
        libffi::low::prep_closure(
            alloc,
            cif.as_raw_ptr(),
            js_callback_trampoline,
            userdata_ptr as *const JsCallbackData,
            code,
        )
        .map_err(|_| VmError::type_error("JSCallback: failed to prepare libffi closure"))?;
    }

    let code_addr = code.as_ptr() as usize;

    let closure = Arc::new(Mutex::new(Some(JsCallbackClosure {
        _closure_alloc: alloc,
        _closure_code: code,
        _cif: cif,
        userdata_ptr,
        _code_ptr: code_addr,
    })));

    // Build the JS object: { ptr, threadsafe, close() }
    let cb_obj = GcRef::new(JsObject::new(Value::null()));
    let _ = cb_obj.set(PropertyKey::string("ptr"), Value::number(code_addr as f64));
    let _ = cb_obj.set(PropertyKey::string("threadsafe"), Value::boolean(false));

    // .close() frees the closure
    let closure_for_close = Arc::clone(&closure);
    let close_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, _args, _ncx| {
        let mut guard = closure_for_close.lock().unwrap();
        if guard.is_some() {
            *guard = None; // Drop the closure (frees libffi alloc + userdata)
        }
        Ok(Value::undefined())
    });

    let fn_proto = ncx
        .global()
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|c| c.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null())));

    let close_fn_obj = GcRef::new(JsObject::new(Value::null()));
    let close_val = Value::native_function_with_proto_and_object(
        close_fn,
        ncx.memory_manager().clone(),
        fn_proto,
        close_fn_obj,
    );
    let _ = cb_obj.set(PropertyKey::string("close"), close_val);

    Ok(Value::object(cb_obj))
}

// ---------------------------------------------------------------------------
// CString class (constructor)
// ---------------------------------------------------------------------------

#[dive(name = "CString", length = 1)]
fn ffi_cstring_ctor(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    require_ffi()?;
    let ptr_val = args.first().copied().unwrap_or(Value::undefined());
    let raw_ptr = to_number(&ptr_val) as usize;
    if raw_ptr == 0 {
        return Err(VmError::type_error("CString: null pointer"));
    }

    let byte_offset = args.get(1).map(|v| to_number(v) as usize).unwrap_or(0);

    let s = unsafe { pointer::read_cstring(raw_ptr, byte_offset) }
        .map_err(|e| VmError::type_error(e.to_string()))?;

    Ok(Value::string(JsString::intern(&s)))
}

// ---------------------------------------------------------------------------
// Public constructor
// ---------------------------------------------------------------------------

/// Create a boxed FFI extension instance for registration.
pub fn otter_ffi_extension() -> Box<dyn OtterExtension> {
    Box::new(OtterFfiExtension)
}
