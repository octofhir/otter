use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

use libffi::middle::{Arg, Cif};
use libloading::Library as DynLib;
use otter_macros::{dive, lodge};
use otter_runtime::{
    NativeFunctionDescriptor, ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError,
    current_capabilities,
};
use otter_vm::object::HeapValueKind;
use otter_vm::payload::{VmTrace, VmValueTracer};

#[derive(Debug, thiserror::Error)]
pub enum FfiError {
    #[error("failed to load library '{path}': {reason}")]
    LibraryLoad { path: String, reason: String },
    #[error("symbol '{name}' not found: {reason}")]
    SymbolNotFound { name: String, reason: String },
    #[error("invalid FFI type: '{name}'")]
    InvalidType { name: String },
    #[error("null pointer dereference")]
    NullPointer,
    #[error("library already closed")]
    LibraryClosed,
    #[error("FFI call failed: {0}")]
    CallFailed(String),
}

pub type FfiResult<T> = Result<T, FfiError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FFIType {
    Char = 0,
    I8 = 1,
    U8 = 2,
    I16 = 3,
    U16 = 4,
    I32 = 5,
    U32 = 6,
    I64 = 7,
    U64 = 8,
    F64 = 9,
    F32 = 10,
    Bool = 11,
    Ptr = 12,
    Void = 13,
    CString = 14,
    I64Fast = 15,
    U64Fast = 16,
    Function = 17,
}

impl FFIType {
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "char" => Some(Self::Char),
            "i8" | "int8_t" => Some(Self::I8),
            "u8" | "uint8_t" => Some(Self::U8),
            "i16" | "int16_t" => Some(Self::I16),
            "u16" | "uint16_t" => Some(Self::U16),
            "i32" | "int32_t" | "int" => Some(Self::I32),
            "u32" | "uint32_t" => Some(Self::U32),
            "i64" | "int64_t" | "i64_fast" => Some(Self::I64),
            "u64" | "uint64_t" | "usize" | "u64_fast" => Some(Self::U64),
            "f64" | "double" => Some(Self::F64),
            "f32" | "float" => Some(Self::F32),
            "bool" => Some(Self::Bool),
            "ptr" | "pointer" => Some(Self::Ptr),
            "void" => Some(Self::Void),
            "cstring" => Some(Self::CString),
            "function" | "fn" | "callback" => Some(Self::Function),
            _ => None,
        }
    }

    pub fn from_u8(value: u8) -> Option<Self> {
        if value <= 17 {
            Some(unsafe { std::mem::transmute::<u8, FFIType>(value) })
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::I8 => "i8",
            Self::U8 => "u8",
            Self::I16 => "i16",
            Self::U16 => "u16",
            Self::I32 => "i32",
            Self::U32 => "u32",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::F64 => "f64",
            Self::F32 => "f32",
            Self::Bool => "bool",
            Self::Ptr => "ptr",
            Self::Void => "void",
            Self::CString => "cstring",
            Self::I64Fast => "i64_fast",
            Self::U64Fast => "u64_fast",
            Self::Function => "function",
        }
    }

    fn to_libffi_type(self) -> libffi::middle::Type {
        use libffi::middle::Type;
        match self {
            Self::Void => Type::void(),
            Self::Char | Self::I8 => Type::i8(),
            Self::U8 | Self::Bool => Type::u8(),
            Self::I16 => Type::i16(),
            Self::U16 => Type::u16(),
            Self::I32 => Type::i32(),
            Self::U32 => Type::u32(),
            Self::I64 | Self::I64Fast => Type::i64(),
            Self::U64 | Self::U64Fast => Type::u64(),
            Self::F32 => Type::f32(),
            Self::F64 => Type::f64(),
            Self::Ptr | Self::CString | Self::Function => Type::pointer(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FfiSignature {
    pub args: Vec<FFIType>,
    pub returns: FFIType,
}

#[derive(Debug)]
struct BoundSymbol {
    ptr: *const (),
    signature: FfiSignature,
}

#[derive(Debug)]
struct FfiLibrary {
    _lib: DynLib,
    symbols: HashMap<String, BoundSymbol>,
}

unsafe impl Send for BoundSymbol {}
unsafe impl Sync for BoundSymbol {}
unsafe impl Send for FfiLibrary {}
unsafe impl Sync for FfiLibrary {}

thread_local! {
    static CURRENT_FFI_RUNTIME: Cell<*mut RuntimeState> = const { Cell::new(std::ptr::null_mut()) };
}

struct FfiRuntimeScope {
    previous: *mut RuntimeState,
}

impl Drop for FfiRuntimeScope {
    fn drop(&mut self) {
        CURRENT_FFI_RUNTIME.with(|slot| slot.set(self.previous));
    }
}

fn enter_ffi_runtime(runtime: &mut RuntimeState) -> FfiRuntimeScope {
    let previous = CURRENT_FFI_RUNTIME.with(|slot| slot.replace(runtime as *mut RuntimeState));
    FfiRuntimeScope { previous }
}

unsafe fn current_ffi_runtime<'a>() -> Option<&'a mut RuntimeState> {
    let ptr = CURRENT_FFI_RUNTIME.with(Cell::get);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &mut *ptr })
    }
}

impl FfiLibrary {
    fn open(path: &str, signatures: &HashMap<String, FfiSignature>) -> FfiResult<Self> {
        let lib = unsafe { DynLib::new(path) }.map_err(|error| FfiError::LibraryLoad {
            path: path.to_string(),
            reason: error.to_string(),
        })?;

        let mut symbols = HashMap::with_capacity(signatures.len());
        for (name, signature) in signatures {
            let ptr: *const () = unsafe {
                let symbol: libloading::Symbol<*const ()> =
                    lib.get(name.as_bytes())
                        .map_err(|error| FfiError::SymbolNotFound {
                            name: name.clone(),
                            reason: error.to_string(),
                        })?;
                *symbol
            };
            symbols.insert(
                name.clone(),
                BoundSymbol {
                    ptr,
                    signature: signature.clone(),
                },
            );
        }

        Ok(Self { _lib: lib, symbols })
    }

    fn symbol(&self, name: &str) -> Option<&BoundSymbol> {
        self.symbols.get(name)
    }
}

#[derive(Debug)]
struct FfiLibraryPayload {
    path: Box<str>,
    state: Arc<Mutex<Option<FfiLibrary>>>,
}

impl VmTrace for FfiLibraryPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug)]
struct FfiSymbolPayload {
    name: Box<str>,
    state: Arc<Mutex<Option<FfiLibrary>>>,
}

impl VmTrace for FfiSymbolPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug)]
struct FfiCallablePayload {
    name: Box<str>,
    fn_ptr: usize,
    signature: FfiSignature,
}

impl VmTrace for FfiCallablePayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

#[derive(Debug)]
struct JsCallbackData {
    callback: RegisterValue,
    arg_types: Vec<FFIType>,
    return_type: FFIType,
}

unsafe impl Send for JsCallbackData {}
unsafe impl Sync for JsCallbackData {}

#[derive(Debug)]
struct JsCallbackClosure {
    alloc: *mut libffi::low::ffi_closure,
    _code: libffi::low::CodePtr,
    _cif: Box<Cif>,
    userdata_ptr: *mut JsCallbackData,
    _code_ptr: usize,
}

impl Drop for JsCallbackClosure {
    fn drop(&mut self) {
        unsafe {
            libffi::low::closure_free(self.alloc);
            drop(Box::from_raw(self.userdata_ptr));
        }
    }
}

unsafe impl Send for JsCallbackClosure {}
unsafe impl Sync for JsCallbackClosure {}

#[derive(Debug)]
struct JsCallbackPayload {
    callback: RegisterValue,
    closure: Arc<Mutex<Option<JsCallbackClosure>>>,
    code_ptr: usize,
}

impl VmTrace for JsCallbackPayload {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.callback.trace(tracer);
    }
}

lodge!(
    ffi_module,
    module_specifiers = ["otter:ffi"],
    default = object,
    functions = [
        ("dlopen", ffi_dlopen),
        ("ptr", ffi_ptr),
        ("CString", ffi_cstring),
        ("toArrayBuffer", ffi_to_array_buffer),
        ("toBuffer", ffi_to_buffer),
        ("CFunction", ffi_cfunction),
        ("linkSymbols", ffi_link_symbols),
        ("JSCallback", ffi_js_callback),
    ],
    values = [
        (
            "suffix",
            RegisterValue::from_object_handle(runtime.alloc_string(platform_suffix()).0)
        ),
        (
            "read",
            RegisterValue::from_object_handle(build_read_namespace(runtime)?.0)
        ),
        (
            "FFIType",
            RegisterValue::from_object_handle(build_ffi_type_object(runtime)?.0)
        ),
    ],
);

#[dive(name = "dlopen", length = 2)]
fn ffi_dlopen(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let path = required_string_arg(runtime, args.first(), "ffi.dlopen: missing library path")?;
    let declarations = required_object_arg(
        runtime,
        args.get(1),
        "ffi.dlopen: symbol declarations must be an object",
    )?;
    let signatures = parse_symbol_declarations(runtime, declarations)?;
    let library =
        FfiLibrary::open(&path, &signatures).map_err(|error| ffi_error(runtime, error))?;
    let shared = Arc::new(Mutex::new(Some(library)));

    let object = runtime.alloc_native_object(FfiLibraryPayload {
        path: path.clone().into_boxed_str(),
        state: shared.clone(),
    });
    let symbols = runtime.alloc_object();

    let mut symbol_names: Vec<_> = signatures.keys().cloned().collect();
    symbol_names.sort();
    for name in symbol_names {
        let symbol = runtime.alloc_native_object(FfiSymbolPayload {
            name: name.clone().into_boxed_str(),
            state: shared.clone(),
        });
        let target = alloc_named_function(runtime, &name, 0, ffi_symbol_call);
        let bound = runtime
            .objects_mut()
            .alloc_bound_function(
                target,
                RegisterValue::from_object_handle(symbol.0),
                Vec::new(),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to bind ffi symbol '{name}': {error:?}").into(),
                )
            })?;
        let property = runtime.intern_property_name(&name);
        runtime
            .objects_mut()
            .set_property(
                symbols,
                property,
                RegisterValue::from_object_handle(bound.0),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install ffi symbol '{name}': {error:?}").into(),
                )
            })?;
    }

    install_method(runtime, object, "close", 0, ffi_library_close)?;
    install_getter(runtime, object, "path", ffi_library_path)?;
    install_getter(runtime, object, "closed", ffi_library_closed)?;
    let symbols_property = runtime.intern_property_name("symbols");
    runtime
        .objects_mut()
        .set_property(
            object,
            symbols_property,
            RegisterValue::from_object_handle(symbols.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi symbols object: {error:?}").into(),
            )
        })?;

    Ok(RegisterValue::from_object_handle(object.0))
}

fn ffi_symbol_call(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let (shared, name) = {
        let payload = match runtime.native_payload_from_value::<FfiSymbolPayload>(this) {
            Ok(payload) => payload,
            Err(_) => return Err(throw_type_error(runtime, "ffi symbol receiver is invalid")),
        };
        (payload.state.clone(), payload.name.clone())
    };
    let state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("ffi symbol mutex poisoned".into()))?;
    let Some(library) = state.as_ref() else {
        return Err(throw_type_error(runtime, "FFI library is closed"));
    };
    let Some(symbol) = library.symbol(&name) else {
        return Err(throw_type_error(runtime, "FFI symbol is not bound"));
    };
    let signature = symbol.signature.clone();
    let fn_ptr = symbol.ptr;
    drop(state);
    let (raw_args, _cstrings) = marshal_args(args, &signature.args, runtime)?;
    let raw = match ffi_call_with_runtime(runtime, fn_ptr, &signature, &raw_args) {
        Ok(raw) => raw,
        Err(error) => return Err(ffi_error(runtime, error)),
    };
    marshal_raw_to_value(raw, signature.returns, runtime)
}

fn ffi_library_close(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = {
        let payload = match runtime.native_payload_mut_from_value::<FfiLibraryPayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "ffi.close: receiver is not an FFI library",
                ));
            }
        };
        payload.state.clone()
    };
    let mut state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("ffi.close mutex poisoned".into()))?;
    state.take();
    Ok(RegisterValue::undefined())
}

fn ffi_library_path(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let path = {
        let payload = match runtime.native_payload_mut_from_value::<FfiLibraryPayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "ffi.path: receiver is not an FFI library",
                ));
            }
        };
        payload.path.clone()
    };
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(path).0,
    ))
}

fn ffi_library_closed(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let shared = {
        let payload = match runtime.native_payload_mut_from_value::<FfiLibraryPayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "ffi.closed: receiver is not an FFI library",
                ));
            }
        };
        payload.state.clone()
    };
    let state = shared
        .lock()
        .map_err(|_| VmNativeCallError::Internal("ffi.closed mutex poisoned".into()))?;
    let closed = state.is_none();
    Ok(RegisterValue::from_bool(closed))
}

fn ffi_read_u8(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_u8(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_i32(i32::from(value)))
}

fn ffi_read_i8(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_i8(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_i32(i32::from(value)))
}

fn ffi_read_u16(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_u16(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_i32(i32::from(value)))
}

fn ffi_read_i16(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_i16(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_i32(i32::from(value)))
}

fn ffi_read_u32(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_u32(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(value as f64))
}

fn ffi_read_i32(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_i32(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_i32(value))
}

fn ffi_read_u64(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_u64(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(value as f64))
}

fn ffi_read_i64(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_i64(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(value as f64))
}

fn ffi_read_f32(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_f32(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(f64::from(value)))
}

fn ffi_read_f64(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_f64(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(value))
}

fn ffi_read_ptr(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_ptr(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(pointer_to_register(value))
}

fn ffi_read_intptr(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_intptr(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_number(value as f64))
}

fn ffi_read_cstring(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = unsafe {
        read_cstring(
            required_ptr(runtime, args.first())?,
            optional_offset(args.get(1))?,
        )
    }
    .map_err(|error| ffi_error(runtime, error))?;
    Ok(RegisterValue::from_object_handle(
        runtime.alloc_string(value).0,
    ))
}

#[dive(name = "ptr", length = 2)]
fn ffi_ptr(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let offset = optional_offset(args.get(1))?;
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(RegisterValue::null());
    }
    if let Some(number) = value.as_number() {
        return Ok(pointer_to_register(
            (number as usize).saturating_add(offset),
        ));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(
            runtime,
            "ffi.ptr: argument must be a number, TypedArray, ArrayBuffer, null, or undefined",
        ));
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::ArrayBuffer) => {
            let data = match runtime.objects().array_buffer_data(handle) {
                Ok(data) => data,
                Err(_) => return Err(throw_type_error(runtime, "ffi.ptr: invalid ArrayBuffer")),
            };
            let Some(data) = data else {
                return Err(throw_type_error(
                    runtime,
                    "ffi.ptr: ArrayBuffer is detached",
                ));
            };
            Ok(pointer_to_register(data.as_ptr() as usize + offset))
        }
        Ok(HeapValueKind::TypedArray) => {
            let viewed_buffer = runtime
                .objects()
                .typed_array_viewed_buffer(handle)
                .map_err(|_| throw_type_error(runtime, "ffi.ptr: invalid TypedArray"))?;
            let byte_offset = runtime
                .objects()
                .typed_array_byte_offset(handle)
                .map_err(|_| throw_type_error(runtime, "ffi.ptr: TypedArray is detached"))?;
            let data = match runtime.objects().array_buffer_data(viewed_buffer) {
                Ok(data) => data,
                Err(_) => {
                    return Err(throw_type_error(
                        runtime,
                        "ffi.ptr: SharedArrayBuffer is not supported yet",
                    ));
                }
            };
            let Some(data) = data else {
                return Err(throw_type_error(
                    runtime,
                    "ffi.ptr: TypedArray buffer is detached",
                ));
            };
            Ok(pointer_to_register(
                data.as_ptr() as usize + byte_offset + offset,
            ))
        }
        _ => Err(throw_type_error(
            runtime,
            "ffi.ptr: argument must be a number, TypedArray, ArrayBuffer, null, or undefined",
        )),
    }
}

#[dive(name = "CString", length = 2)]
fn ffi_cstring(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let ptr = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if ptr == RegisterValue::undefined() || ptr == RegisterValue::null() {
        return Ok(RegisterValue::null());
    }
    ffi_read_cstring(this, args, runtime)
}

#[dive(name = "toArrayBuffer", length = 3)]
fn ffi_to_array_buffer(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let ptr = required_ptr(runtime, args.first())?;
    let byte_offset = optional_offset(args.get(1))?;
    let byte_length = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_number)
        .ok_or_else(|| throw_type_error(runtime, "ffi.toArrayBuffer: byteLength is required"))?
        as usize;
    let addr = ptr
        .checked_add(byte_offset)
        .ok_or_else(|| ffi_error(runtime, FfiError::NullPointer))?;
    let data = unsafe { std::slice::from_raw_parts(addr as *const u8, byte_length) }.to_vec();
    let prototype = Some(runtime.intrinsics().array_buffer_prototype());
    let buffer = runtime
        .objects_mut()
        .alloc_array_buffer_with_data(data, prototype);
    Ok(RegisterValue::from_object_handle(buffer.0))
}

#[dive(name = "toBuffer", length = 3)]
fn ffi_to_buffer(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    ffi_to_array_buffer(this, args, runtime)
}

#[dive(name = "CFunction", length = 1)]
fn ffi_cfunction(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let definition = required_object_arg(
        runtime,
        args.first(),
        "ffi.CFunction: definition must be an object with ptr, args, and returns",
    )?;
    let (fn_ptr, signature) = parse_callable_definition(runtime, definition, "ffi.CFunction")?;
    let callable = build_direct_callable(runtime, "CFunction", fn_ptr, signature)?;
    Ok(RegisterValue::from_object_handle(callable.0))
}

#[dive(name = "linkSymbols", length = 1)]
fn ffi_link_symbols(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let declarations = required_object_arg(
        runtime,
        args.first(),
        "ffi.linkSymbols: symbol declarations must be an object",
    )?;
    let symbols = runtime.alloc_object();
    let mut entries = Vec::new();
    for key in runtime.enumerable_own_property_keys(declarations)? {
        let Some(name) = runtime.property_names().get(key).map(str::to_owned) else {
            continue;
        };
        let declaration = runtime.own_property_value(declarations, key)?;
        let definition = declaration
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                throw_type_error(
                    runtime,
                    &format!("ffi.linkSymbols: declaration for '{name}' must be an object"),
                )
            })?;
        let (fn_ptr, signature) =
            parse_callable_definition(runtime, definition, "ffi.linkSymbols")?;
        entries.push((name, fn_ptr, signature));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, fn_ptr, signature) in entries {
        let callable = build_direct_callable(runtime, &name, fn_ptr, signature)?;
        let property = runtime.intern_property_name(&name);
        runtime
            .objects_mut()
            .set_property(
                symbols,
                property,
                RegisterValue::from_object_handle(callable.0),
            )
            .map_err(|error| {
                VmNativeCallError::Internal(
                    format!("failed to install ffi linked symbol '{name}': {error:?}").into(),
                )
            })?;
    }

    let library = runtime.alloc_object();
    let symbols_property = runtime.intern_property_name("symbols");
    runtime
        .objects_mut()
        .set_property(
            library,
            symbols_property,
            RegisterValue::from_object_handle(symbols.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi linked symbols object: {error:?}").into(),
            )
        })?;
    install_method(runtime, library, "close", 0, ffi_linked_symbols_close)?;

    Ok(RegisterValue::from_object_handle(library.0))
}

fn ffi_linked_symbols_close(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::undefined())
}

fn ffi_bound_callable_call(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let (name, fn_ptr, signature) = {
        let payload = match runtime.native_payload_from_value::<FfiCallablePayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "ffi callable receiver is invalid",
                ));
            }
        };
        (
            payload.name.clone(),
            payload.fn_ptr as *const (),
            payload.signature.clone(),
        )
    };
    let (raw_args, _cstrings) = marshal_args(args, &signature.args, runtime)?;
    let raw = ffi_call_with_runtime(runtime, fn_ptr, &signature, &raw_args).map_err(|error| {
        throw_type_error(runtime, &format!("ffi call '{name}' failed: {error}"))
    })?;
    marshal_raw_to_value(raw, signature.returns, runtime)
}

#[dive(name = "JSCallback", length = 2)]
fn ffi_js_callback(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    require_ffi(runtime)?;
    let callback = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let callback_handle = callback
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| throw_type_error(runtime, "ffi.JSCallback: callback must be a function"))?;
    if !runtime.objects().is_callable(callback_handle) {
        return Err(throw_type_error(
            runtime,
            "ffi.JSCallback: callback must be a function",
        ));
    }

    let definition = required_object_arg(
        runtime,
        args.get(1),
        "ffi.JSCallback: definition must be an object with args and returns",
    )?;
    let signature = parse_signature_definition(runtime, definition, "ffi.JSCallback")?;

    let cif = Box::new(Cif::new(
        signature.args.iter().map(|ty| ty.to_libffi_type()),
        signature.returns.to_libffi_type(),
    ));
    let userdata = Box::new(JsCallbackData {
        callback,
        arg_types: signature.args.clone(),
        return_type: signature.returns,
    });
    let userdata_ptr = Box::into_raw(userdata);
    let (alloc, code) = libffi::low::closure_alloc();
    if alloc.is_null() {
        unsafe {
            drop(Box::from_raw(userdata_ptr));
        }
        return Err(throw_type_error(
            runtime,
            "ffi.JSCallback: failed to allocate callback closure",
        ));
    }
    unsafe {
        libffi::low::prep_closure(
            alloc,
            cif.as_raw_ptr(),
            ffi_js_callback_trampoline,
            userdata_ptr as *const JsCallbackData,
            code,
        )
        .map_err(|_| {
            drop(Box::from_raw(userdata_ptr));
            libffi::low::closure_free(alloc);
            throw_type_error(
                runtime,
                "ffi.JSCallback: failed to prepare callback closure",
            )
        })?;
    }

    let code_ptr = code.as_ptr() as usize;
    let callback_state = Arc::new(Mutex::new(Some(JsCallbackClosure {
        alloc,
        _code: code,
        _cif: cif,
        userdata_ptr,
        _code_ptr: code_ptr,
    })));
    let object = runtime.alloc_native_object(JsCallbackPayload {
        callback,
        closure: callback_state,
        code_ptr,
    });
    let ptr_property = runtime.intern_property_name("ptr");
    runtime
        .objects_mut()
        .set_property(object, ptr_property, pointer_to_register(code_ptr))
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi.JSCallback ptr: {error:?}").into(),
            )
        })?;
    let threadsafe_property = runtime.intern_property_name("threadsafe");
    runtime
        .objects_mut()
        .set_property(object, threadsafe_property, RegisterValue::from_bool(false))
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi.JSCallback threadsafe flag: {error:?}").into(),
            )
        })?;
    install_method(runtime, object, "close", 0, ffi_js_callback_close)?;

    Ok(RegisterValue::from_object_handle(object.0))
}

fn ffi_js_callback_close(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let closure = {
        let payload = match runtime.native_payload_mut_from_value::<JsCallbackPayload>(this) {
            Ok(payload) => payload,
            Err(_) => {
                return Err(throw_type_error(
                    runtime,
                    "ffi.JSCallback.close receiver is invalid",
                ));
            }
        };
        payload.closure.clone()
    };
    let mut state = closure
        .lock()
        .map_err(|_| VmNativeCallError::Internal("ffi.JSCallback mutex poisoned".into()))?;
    state.take();
    Ok(RegisterValue::undefined())
}

unsafe extern "C" fn ffi_js_callback_trampoline(
    _cif: &libffi::low::ffi_cif,
    result: &mut u64,
    args: *const *const std::ffi::c_void,
    userdata: &JsCallbackData,
) {
    let runtime = match unsafe { current_ffi_runtime() } {
        Some(runtime) => runtime,
        None => {
            *result = 0;
            return;
        }
    };
    let Some(callback) = userdata.callback.as_object_handle().map(ObjectHandle) else {
        *result = 0;
        return;
    };
    if !runtime.objects().is_callable(callback) {
        *result = 0;
        return;
    }

    let mut callback_args = Vec::with_capacity(userdata.arg_types.len());
    for (index, ty) in userdata.arg_types.iter().copied().enumerate() {
        let raw_arg = match unsafe { *args.add(index) } {
            ptr if ptr.is_null() => 0,
            ptr => match ty {
                FFIType::Char | FFIType::I8 => unsafe { *(ptr as *const i8) as u64 },
                FFIType::U8 | FFIType::Bool => unsafe { *(ptr as *const u8) as u64 },
                FFIType::I16 => unsafe { *(ptr as *const i16) as u64 },
                FFIType::U16 => unsafe { *(ptr as *const u16) as u64 },
                FFIType::I32 => unsafe { *(ptr as *const i32) as u64 },
                FFIType::U32 => unsafe { *(ptr as *const u32) as u64 },
                FFIType::I64 | FFIType::I64Fast => unsafe { *(ptr as *const i64) as u64 },
                FFIType::U64 | FFIType::U64Fast => unsafe { *(ptr as *const u64) },
                FFIType::F32 => unsafe { (*(ptr as *const f32)).to_bits() as u64 },
                FFIType::F64 => unsafe { (*(ptr as *const f64)).to_bits() },
                FFIType::Ptr | FFIType::CString | FFIType::Function => unsafe {
                    *(ptr as *const usize) as u64
                },
                FFIType::Void => 0,
            },
        };
        match marshal_raw_to_value(raw_arg, ty, runtime) {
            Ok(value) => callback_args.push(value),
            Err(_) => callback_args.push(RegisterValue::undefined()),
        }
    }

    let callback_result =
        match runtime.call_callable(callback, RegisterValue::undefined(), &callback_args) {
            Ok(value) => value,
            Err(_) => {
                *result = 0;
                return;
            }
        };
    let mut cstrings = Vec::new();
    *result = match marshal_value_to_raw(
        callback_result,
        userdata.return_type,
        runtime,
        &mut cstrings,
    ) {
        Ok(raw) => raw,
        Err(_) => 0,
    };
}

fn build_direct_callable(
    runtime: &mut RuntimeState,
    name: &str,
    fn_ptr: usize,
    signature: FfiSignature,
) -> Result<ObjectHandle, VmNativeCallError> {
    let payload = runtime.alloc_native_object(FfiCallablePayload {
        name: name.to_string().into_boxed_str(),
        fn_ptr,
        signature: signature.clone(),
    });
    let target = alloc_named_function(
        runtime,
        name,
        u16::try_from(signature.args.len()).unwrap_or(u16::MAX),
        ffi_bound_callable_call,
    );
    runtime
        .objects_mut()
        .alloc_bound_function(
            target,
            RegisterValue::from_object_handle(payload.0),
            Vec::new(),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to bind ffi callable '{name}': {error:?}").into(),
            )
        })
}

fn parse_callable_definition(
    runtime: &mut RuntimeState,
    definition: ObjectHandle,
    context: &str,
) -> Result<(usize, FfiSignature), VmNativeCallError> {
    let ptr_property = runtime.intern_property_name("ptr");
    let pointer = runtime
        .own_property_value(definition, ptr_property)
        .map_err(|_| throw_type_error(runtime, &format!("{context}: definition must have ptr")))?;
    let fn_ptr = function_pointer_like(pointer, runtime)?;
    if fn_ptr == 0 {
        return Err(throw_type_error(
            runtime,
            &format!("{context}: ptr must be non-null"),
        ));
    }
    let signature = parse_signature_definition(runtime, definition, context)?;
    Ok((fn_ptr, signature))
}

fn parse_signature_definition(
    runtime: &mut RuntimeState,
    definition: ObjectHandle,
    _context: &str,
) -> Result<FfiSignature, VmNativeCallError> {
    let args_property = runtime.intern_property_name("args");
    let returns_property = runtime.intern_property_name("returns");
    let args = runtime
        .own_property_value(definition, args_property)
        .unwrap_or_else(|_| RegisterValue::undefined());
    let returns = runtime
        .own_property_value(definition, returns_property)
        .unwrap_or_else(|_| RegisterValue::undefined());
    Ok(FfiSignature {
        args: parse_ffi_args(runtime, args)?,
        returns: if returns == RegisterValue::undefined() {
            FFIType::Void
        } else {
            parse_ffi_type_value(runtime, returns)?
        },
    })
}

fn parse_symbol_declarations(
    runtime: &mut RuntimeState,
    declarations: ObjectHandle,
) -> Result<HashMap<String, FfiSignature>, VmNativeCallError> {
    let mut signatures = HashMap::new();
    for key in runtime.enumerable_own_property_keys(declarations)? {
        let Some(name) = runtime.property_names().get(key).map(str::to_owned) else {
            continue;
        };
        let declaration = runtime.own_property_value(declarations, key)?;
        let decl_handle = declaration
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                throw_type_error(
                    runtime,
                    &format!("ffi.dlopen: declaration for '{name}' must be an object"),
                )
            })?;
        let args_property = runtime.intern_property_name("args");
        let returns_property = runtime.intern_property_name("returns");
        let args_value = runtime
            .own_property_value(decl_handle, args_property)
            .unwrap_or_else(|_| RegisterValue::undefined());
        let returns_value = runtime
            .own_property_value(decl_handle, returns_property)
            .unwrap_or_else(|_| RegisterValue::undefined());
        signatures.insert(
            name,
            FfiSignature {
                args: parse_ffi_args(runtime, args_value)?,
                returns: if returns_value == RegisterValue::undefined() {
                    FFIType::Void
                } else {
                    parse_ffi_type_value(runtime, returns_value)?
                },
            },
        );
    }
    Ok(signatures)
}

fn parse_ffi_args(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<Vec<FFIType>, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }
    let handle = value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| throw_type_error(runtime, "ffi args must be an array"))?;
    if runtime.objects().kind(handle) != Ok(HeapValueKind::Array) {
        return Err(throw_type_error(runtime, "ffi args must be an array"));
    }
    let values = runtime.array_to_args(handle)?;
    values
        .into_iter()
        .map(|value| parse_ffi_type_value(runtime, value))
        .collect()
}

fn parse_ffi_type_value(
    runtime: &mut RuntimeState,
    value: RegisterValue,
) -> Result<FFIType, VmNativeCallError> {
    if let Some(number) = value.as_number() {
        return FFIType::from_u8(number as u8).ok_or_else(|| {
            throw_type_error(runtime, &format!("invalid FFI type number: {number}"))
        });
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && runtime.objects().kind(handle) == Ok(HeapValueKind::String)
    {
        let string = runtime.js_to_string_infallible(value).into_string();
        return FFIType::from_name(&string)
            .ok_or_else(|| throw_type_error(runtime, &format!("invalid FFI type: '{string}'")));
    }
    Err(throw_type_error(
        runtime,
        "FFI type must be a string or number",
    ))
}

fn marshal_args(
    args: &[RegisterValue],
    expected: &[FFIType],
    runtime: &mut RuntimeState,
) -> Result<(Vec<u64>, Vec<CString>), VmNativeCallError> {
    let mut raw = Vec::with_capacity(expected.len());
    let mut cstrings = Vec::new();
    for (index, ty) in expected.iter().copied().enumerate() {
        let value = args
            .get(index)
            .copied()
            .unwrap_or_else(RegisterValue::undefined);
        raw.push(marshal_value_to_raw(value, ty, runtime, &mut cstrings)?);
    }
    Ok((raw, cstrings))
}

fn marshal_value_to_raw(
    value: RegisterValue,
    ty: FFIType,
    runtime: &mut RuntimeState,
    cstrings: &mut Vec<CString>,
) -> Result<u64, VmNativeCallError> {
    match ty {
        FFIType::I8 | FFIType::Char => Ok(number_like(value, runtime)? as i8 as u64),
        FFIType::U8 => Ok(number_like(value, runtime)? as u8 as u64),
        FFIType::Bool => Ok(u64::from(boolean_like(value))),
        FFIType::I16 => Ok(number_like(value, runtime)? as i16 as u64),
        FFIType::U16 => Ok(number_like(value, runtime)? as u16 as u64),
        FFIType::I32 => Ok(number_like(value, runtime)? as i32 as u64),
        FFIType::U32 => Ok(number_like(value, runtime)? as u32 as u64),
        FFIType::I64 | FFIType::I64Fast => Ok(number_like(value, runtime)? as i64 as u64),
        FFIType::U64 | FFIType::U64Fast => Ok(number_like(value, runtime)? as u64),
        FFIType::F32 => Ok((number_like(value, runtime)? as f32).to_bits() as u64),
        FFIType::F64 => Ok(number_like(value, runtime)?.to_bits()),
        FFIType::Ptr => Ok(pointer_like(value, runtime)? as u64),
        FFIType::CString => {
            if value == RegisterValue::undefined() || value == RegisterValue::null() {
                return Ok(0);
            }
            let string = runtime.js_to_string_infallible(value).into_string();
            let cstring = CString::new(string).map_err(|_| {
                throw_type_error(runtime, "FFI CString arguments cannot contain NUL bytes")
            })?;
            let ptr = cstring.as_ptr() as usize;
            cstrings.push(cstring);
            Ok(ptr as u64)
        }
        FFIType::Function => Ok(function_pointer_like(value, runtime)? as u64),
        FFIType::Void => Err(throw_type_error(
            runtime,
            "void is not a valid FFI argument type",
        )),
    }
}

fn marshal_raw_to_value(
    raw: u64,
    ty: FFIType,
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    match ty {
        FFIType::Void => Ok(RegisterValue::undefined()),
        FFIType::Bool => Ok(RegisterValue::from_bool(raw != 0)),
        FFIType::I8 | FFIType::Char => Ok(RegisterValue::from_i32(i32::from(raw as i8))),
        FFIType::U8 => Ok(RegisterValue::from_i32(i32::from(raw as u8))),
        FFIType::I16 => Ok(RegisterValue::from_i32(i32::from(raw as i16))),
        FFIType::U16 => Ok(RegisterValue::from_i32(i32::from(raw as u16))),
        FFIType::I32 => Ok(RegisterValue::from_i32(raw as i32)),
        FFIType::U32 => Ok(RegisterValue::from_number((raw as u32) as f64)),
        FFIType::I64 | FFIType::I64Fast => Ok(RegisterValue::from_number((raw as i64) as f64)),
        FFIType::U64 | FFIType::U64Fast => Ok(RegisterValue::from_number(raw as f64)),
        FFIType::F32 => Ok(RegisterValue::from_number(f64::from(f32::from_bits(
            raw as u32,
        )))),
        FFIType::F64 => Ok(RegisterValue::from_number(f64::from_bits(raw))),
        FFIType::Ptr | FFIType::Function => Ok(pointer_to_register(raw as usize)),
        FFIType::CString => {
            if raw == 0 {
                return Ok(RegisterValue::null());
            }
            let string = unsafe { CStr::from_ptr(raw as *const i8) }
                .to_string_lossy()
                .into_owned();
            Ok(RegisterValue::from_object_handle(
                runtime.alloc_string(string).0,
            ))
        }
    }
}

unsafe fn ffi_call(fn_ptr: *const (), signature: &FfiSignature, args: &[u64]) -> FfiResult<u64> {
    let arg_types: Vec<_> = signature
        .args
        .iter()
        .map(|ty| ty.to_libffi_type())
        .collect();
    let cif = Cif::new(arg_types, signature.returns.to_libffi_type());
    let mut storage = args.to_vec();
    let ffi_args: Vec<Arg> = storage.iter_mut().map(|value| Arg::new(&*value)).collect();
    let code_ptr = libffi::middle::CodePtr::from_ptr(fn_ptr as *const _);
    let result = match signature.returns {
        FFIType::Void => {
            unsafe { cif.call::<()>(code_ptr, &ffi_args) };
            0
        }
        FFIType::Char | FFIType::I8 => unsafe { cif.call::<i8>(code_ptr, &ffi_args) as u64 },
        FFIType::U8 | FFIType::Bool => unsafe { cif.call::<u8>(code_ptr, &ffi_args) as u64 },
        FFIType::I16 => unsafe { cif.call::<i16>(code_ptr, &ffi_args) as u64 },
        FFIType::U16 => unsafe { cif.call::<u16>(code_ptr, &ffi_args) as u64 },
        FFIType::I32 => unsafe { cif.call::<i32>(code_ptr, &ffi_args) as u64 },
        FFIType::U32 => unsafe { cif.call::<u32>(code_ptr, &ffi_args) as u64 },
        FFIType::I64 | FFIType::I64Fast => unsafe { cif.call::<i64>(code_ptr, &ffi_args) as u64 },
        FFIType::U64 | FFIType::U64Fast => unsafe { cif.call::<u64>(code_ptr, &ffi_args) },
        FFIType::F32 => unsafe { cif.call::<f32>(code_ptr, &ffi_args).to_bits() as u64 },
        FFIType::F64 => unsafe { cif.call::<f64>(code_ptr, &ffi_args).to_bits() },
        FFIType::Ptr | FFIType::CString | FFIType::Function => unsafe {
            cif.call::<usize>(code_ptr, &ffi_args) as u64
        },
    };
    Ok(result)
}

fn ffi_call_with_runtime(
    runtime: &mut RuntimeState,
    fn_ptr: *const (),
    signature: &FfiSignature,
    args: &[u64],
) -> FfiResult<u64> {
    let _scope = enter_ffi_runtime(runtime);
    unsafe { ffi_call(fn_ptr, signature, args) }
}

unsafe fn read_u8(ptr: usize, offset: usize) -> FfiResult<u8> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { *(addr as *const u8) })
}

unsafe fn read_i8(ptr: usize, offset: usize) -> FfiResult<i8> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { *(addr as *const i8) })
}

unsafe fn read_u16(ptr: usize, offset: usize) -> FfiResult<u16> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u16) })
}

unsafe fn read_i16(ptr: usize, offset: usize) -> FfiResult<i16> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i16) })
}

unsafe fn read_u32(ptr: usize, offset: usize) -> FfiResult<u32> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u32) })
}

unsafe fn read_i32(ptr: usize, offset: usize) -> FfiResult<i32> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i32) })
}

unsafe fn read_u64(ptr: usize, offset: usize) -> FfiResult<u64> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u64) })
}

unsafe fn read_i64(ptr: usize, offset: usize) -> FfiResult<i64> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i64) })
}

unsafe fn read_f32(ptr: usize, offset: usize) -> FfiResult<f32> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const f32) })
}

unsafe fn read_f64(ptr: usize, offset: usize) -> FfiResult<f64> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const f64) })
}

unsafe fn read_ptr(ptr: usize, offset: usize) -> FfiResult<usize> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const usize) })
}

unsafe fn read_intptr(ptr: usize, offset: usize) -> FfiResult<isize> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const isize) })
}

unsafe fn read_cstring(ptr: usize, offset: usize) -> FfiResult<String> {
    let addr = checked_addr(ptr, offset)?;
    let cstr = unsafe { CStr::from_ptr(addr as *const i8) };
    Ok(cstr.to_string_lossy().into_owned())
}

fn checked_addr(ptr: usize, offset: usize) -> FfiResult<usize> {
    let addr = ptr.checked_add(offset).ok_or(FfiError::NullPointer)?;
    if addr == 0 {
        return Err(FfiError::NullPointer);
    }
    Ok(addr)
}

fn platform_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}

fn build_read_namespace(runtime: &mut RuntimeState) -> Result<ObjectHandle, String> {
    let namespace = runtime.alloc_object();
    install_function(namespace, runtime, "u8", 2, ffi_read_u8)?;
    install_function(namespace, runtime, "i8", 2, ffi_read_i8)?;
    install_function(namespace, runtime, "u16", 2, ffi_read_u16)?;
    install_function(namespace, runtime, "i16", 2, ffi_read_i16)?;
    install_function(namespace, runtime, "u32", 2, ffi_read_u32)?;
    install_function(namespace, runtime, "i32", 2, ffi_read_i32)?;
    install_function(namespace, runtime, "u64", 2, ffi_read_u64)?;
    install_function(namespace, runtime, "i64", 2, ffi_read_i64)?;
    install_function(namespace, runtime, "f32", 2, ffi_read_f32)?;
    install_function(namespace, runtime, "f64", 2, ffi_read_f64)?;
    install_function(namespace, runtime, "ptr", 2, ffi_read_ptr)?;
    install_function(namespace, runtime, "intptr", 2, ffi_read_intptr)?;
    install_function(namespace, runtime, "cstring", 2, ffi_read_cstring)?;
    Ok(namespace)
}

fn build_ffi_type_object(runtime: &mut RuntimeState) -> Result<ObjectHandle, String> {
    let object = runtime.alloc_object();
    for ty in [
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
    ] {
        let property = runtime.intern_property_name(ty.name());
        runtime
            .objects_mut()
            .set_property(object, property, RegisterValue::from_i32(ty as u8 as i32))
            .map_err(|error| format!("failed to install FFIType.{}: {error:?}", ty.name()))?;
    }
    Ok(object)
}

fn install_function(
    target: ObjectHandle,
    runtime: &mut RuntimeState,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> Result<(), String> {
    let function = alloc_named_function(runtime, name, arity, callback);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(
            target,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map(|_| ())
        .map_err(|error| format!("failed to install otter:ffi function '{name}': {error:?}"))
}

fn install_method(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> Result<(), VmNativeCallError> {
    let function = alloc_named_function(runtime, name, arity, callback);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .set_property(
            target,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi method '{name}': {error:?}").into(),
            )
        })?;
    Ok(())
}

fn install_getter(
    runtime: &mut RuntimeState,
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> Result<(), VmNativeCallError> {
    let descriptor = NativeFunctionDescriptor::getter(name, callback);
    let getter_id = runtime.register_native_function(descriptor);
    let getter = runtime.alloc_host_function(getter_id);
    let property = runtime.intern_property_name(name);
    runtime
        .objects_mut()
        .define_accessor(target, property, Some(getter), None)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("failed to install ffi getter '{name}': {error:?}").into(),
            )
        })?;
    Ok(())
}

fn alloc_named_function(
    runtime: &mut RuntimeState,
    name: &str,
    arity: u16,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> ObjectHandle {
    let descriptor = NativeFunctionDescriptor::method(name, arity, callback);
    let function = runtime.register_native_function(descriptor);
    runtime.alloc_host_function(function)
}

fn required_string_arg(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
    message: &str,
) -> Result<String, VmNativeCallError> {
    let value = *value.ok_or_else(|| throw_type_error(runtime, message))?;
    Ok(runtime.js_to_string_infallible(value).into_string())
}

fn required_object_arg(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
    message: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let value = *value.ok_or_else(|| throw_type_error(runtime, message))?;
    value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| throw_type_error(runtime, message))
}

fn required_ptr(
    runtime: &mut RuntimeState,
    value: Option<&RegisterValue>,
) -> Result<usize, VmNativeCallError> {
    let value =
        *value.ok_or_else(|| throw_type_error(runtime, "ffi pointer argument is required"))?;
    let ptr = pointer_like(value, runtime)?;
    if ptr == 0 {
        return Err(ffi_error(runtime, FfiError::NullPointer));
    }
    Ok(ptr)
}

fn optional_offset(value: Option<&RegisterValue>) -> Result<usize, VmNativeCallError> {
    Ok(match value.copied() {
        Some(value) if value != RegisterValue::undefined() && value != RegisterValue::null() => {
            value.as_number().unwrap_or_default() as usize
        }
        _ => 0,
    })
}

fn number_like(value: RegisterValue, runtime: &mut RuntimeState) -> Result<f64, VmNativeCallError> {
    if let Some(number) = value.as_number() {
        Ok(number)
    } else if let Some(boolean) = value.as_bool() {
        Ok(if boolean { 1.0 } else { 0.0 })
    } else {
        Err(throw_type_error(
            runtime,
            "FFI numeric argument must be a number",
        ))
    }
}

fn boolean_like(value: RegisterValue) -> bool {
    value
        .as_bool()
        .unwrap_or_else(|| value.as_number().unwrap_or(0.0) != 0.0)
}

fn pointer_like(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<usize, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(0);
    }
    if let Some(number) = value.as_number() {
        return Ok(number as usize);
    }
    Err(throw_type_error(
        runtime,
        "FFI pointer argument must be a number, null, or undefined",
    ))
}

fn function_pointer_like(
    value: RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<usize, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(0);
    }
    if let Some(number) = value.as_number() {
        return Ok(number as usize);
    }
    if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
        if let Some(code_ptr) = {
            match runtime.native_payload_from_value::<JsCallbackPayload>(&value) {
                Ok(payload) => match payload.closure.lock() {
                    Ok(state) if state.is_some() => Some(payload.code_ptr),
                    Ok(_) => None,
                    Err(_) => None,
                },
                Err(_) => None,
            }
        } {
            return Ok(code_ptr);
        }

        let ptr_property = runtime.intern_property_name("ptr");
        if let Ok(ptr_value) = runtime.own_property_value(handle, ptr_property)
            && ptr_value != RegisterValue::undefined()
        {
            return pointer_like(ptr_value, runtime);
        }
    }
    Err(throw_type_error(
        runtime,
        "FFI function pointer must be a number, JSCallback, object with ptr, null, or undefined",
    ))
}

fn pointer_to_register(value: usize) -> RegisterValue {
    if value == 0 {
        RegisterValue::null()
    } else {
        RegisterValue::from_number(value as f64)
    }
}

fn require_ffi(runtime: &mut RuntimeState) -> Result<(), VmNativeCallError> {
    current_capabilities(runtime)
        .require_ffi()
        .map_err(|error| throw_type_error(runtime, &error.to_string()))
}

fn throw_type_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(_) => VmNativeCallError::Internal(message.into()),
    }
}

fn ffi_error(runtime: &mut RuntimeState, error: FfiError) -> VmNativeCallError {
    throw_type_error(runtime, &error.to_string())
}
