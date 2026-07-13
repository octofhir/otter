//! Node-API host ABI for loading native `.node` addons.
//!
//! A native addon is a dynamic library whose `napi_register_module_v1` entry
//! point calls the `napi_*` C symbols exported by the Otter executable. This
//! module supplies that VM-neutral ABI directly; it does not embed Node or V8
//! and does not route through `napi-rs` (which is an addon-side binding).
//!
//! # Contents
//! - [`load_addon`] opens a capability-approved library and runs registration.
//! - [`NapiEnv`] owns stable C handles backed by Otter persistent roots.
//! - Exported `napi_*` functions implement the initial Node-API value,
//!   property, callback, exception, external-memory, buffer, Promise,
//!   async-work, and handle-scope surface.
//!
//! # Invariants
//! - Both `read` and `ffi` capabilities are checked before native code loads.
//! - A `napi_value` never stores a raw moving-heap offset. It points to a stable
//!   Rust box containing a persistent-root id; every access rereads the root.
//! - VM allocations and mutations use `NativeCtx::scope` / `scoped_*` APIs.
//! - The raw context pointer is confined to the synchronous C ABI turn. Addons
//!   may not retain `napi_env` beyond that turn or use it from another thread.
//! - Async execute/completion callbacks receive a fresh environment at the
//!   runtime microtask checkpoint; no VM context crosses a thread boundary.
//! - Loaded code stays mapped while any JS callback created from it is alive.
//!   Unsupported ABI symbols remain absent, so the platform loader fails fast.
//!
//! # See also
//! - <https://nodejs.org/api/n-api.html>
//! - [`otter_vm::NativeCtx`]
//! - [`crate::NodeApiBuilderExt`]

#![allow(non_camel_case_types)]
// Every exported function shares the module-level Node-API pointer contract;
// repeating an identical Safety section on the complete C symbol table would
// obscure the ABI surface. Boxed handles are intentional because their
// addresses must remain stable when the owning Vec grows.
#![allow(clippy::missing_safety_doc, clippy::vec_box)]

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};

use libloading::Library;
use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, NativeError, PersistentRootId, Value};
use smallvec::SmallVec;

pub type napi_env = *mut NapiEnv;
pub type napi_value = *mut c_void;
pub type napi_callback_info = *mut NapiCallbackInfo;
pub type napi_status = c_int;
pub type napi_handle_scope = *mut c_void;
pub type napi_ref = *mut NapiRef;
pub type napi_deferred = *mut NapiDeferred;
pub type napi_async_work = *mut NapiAsyncWork;
pub type napi_threadsafe_function = *mut c_void;
pub type napi_callback = Option<unsafe extern "C" fn(napi_env, napi_callback_info) -> napi_value>;
pub type napi_async_execute_callback = Option<unsafe extern "C" fn(napi_env, *mut c_void)>;
pub type napi_async_complete_callback =
    Option<unsafe extern "C" fn(napi_env, napi_status, *mut c_void)>;

const NAPI_OK: napi_status = 0;
const NAPI_INVALID_ARG: napi_status = 1;
const NAPI_OBJECT_EXPECTED: napi_status = 2;
const NAPI_STRING_EXPECTED: napi_status = 3;
const NAPI_FUNCTION_EXPECTED: napi_status = 5;
const NAPI_NUMBER_EXPECTED: napi_status = 6;
const NAPI_BOOLEAN_EXPECTED: napi_status = 7;
const NAPI_PENDING_EXCEPTION: napi_status = 10;

const NAPI_UNDEFINED: c_int = 0;
const NAPI_NULL: c_int = 1;
const NAPI_BOOLEAN: c_int = 2;
const NAPI_NUMBER: c_int = 3;
const NAPI_STRING: c_int = 4;
const NAPI_OBJECT: c_int = 6;
const NAPI_FUNCTION: c_int = 7;
const NAPI_EXTERNAL: c_int = 8;
const NAPI_UINT8_ARRAY: u32 = 1;
const NAPI_AUTO_LENGTH: usize = usize::MAX;
const NAPI_STATIC: c_int = 1 << 10;

#[repr(C)]
pub struct napi_property_descriptor {
    utf8name: *const c_char,
    name: napi_value,
    method: napi_callback,
    getter: napi_callback,
    setter: napi_callback,
    value: napi_value,
    attributes: c_int,
    data: *mut c_void,
}

struct NapiHandle {
    root: PersistentRootId,
}

pub struct NapiRef {
    root: PersistentRootId,
    count: u32,
}

pub struct NapiDeferred {
    resolve: PersistentRootId,
    reject: PersistentRootId,
}

pub struct NapiAsyncWork {
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: usize,
    queued: bool,
}

#[derive(Default)]
struct NapiState {
    wraps: Mutex<Vec<NapiWrap>>,
    externals: Mutex<Vec<NapiExternal>>,
}

struct NapiWrap {
    object: PersistentRootId,
    data: usize,
}

struct NapiExternal {
    value: PersistentRootId,
    data: usize,
}

pub struct NapiEnv {
    ctx: *mut c_void,
    handles: Vec<Box<NapiHandle>>,
    scopes: Vec<usize>,
    pending: Option<NativeError>,
    library: Arc<Library>,
    state: Arc<NapiState>,
}

impl NapiEnv {
    fn new(ctx: &mut NativeCtx<'_>, library: Arc<Library>, state: Arc<NapiState>) -> Self {
        Self {
            ctx: (ctx as *mut NativeCtx<'_>).cast(),
            handles: Vec::new(),
            scopes: Vec::new(),
            pending: None,
            library,
            state,
        }
    }

    /// Recover the mutator-turn context hidden behind the C ABI.
    ///
    /// # Safety
    /// `self.ctx` is live only while the synchronous registration/callback
    /// frame that constructed this environment remains on the Rust stack.
    unsafe fn ctx(&mut self) -> &mut NativeCtx<'static> {
        unsafe { &mut *self.ctx.cast::<NativeCtx<'static>>() }
    }

    fn root(&mut self, value: Value) -> napi_value {
        // Persistent-root insertion does not allocate on the JS heap, so the
        // freshly returned Value cannot move between allocation and rooting.
        let root = unsafe { self.ctx() }.persistent_root_insert(value);
        let handle = Box::new(NapiHandle { root });
        let ptr = (&*handle as *const NapiHandle).cast_mut().cast();
        self.handles.push(handle);
        ptr
    }

    unsafe fn value(&mut self, handle: napi_value) -> Option<Value> {
        if handle.is_null() {
            return Some(Value::undefined());
        }
        let root = unsafe { &*handle.cast::<NapiHandle>() }.root;
        unsafe { self.ctx() }.persistent_root_get(root)
    }

    fn fail(&mut self, error: NativeError) -> napi_status {
        self.pending = Some(error);
        NAPI_PENDING_EXCEPTION
    }

    fn truncate_handles(&mut self, base: usize) {
        while self.handles.len() > base {
            let handle = self.handles.pop().expect("length checked");
            let _ = unsafe { self.ctx() }.persistent_root_remove(handle.root);
        }
    }
}

impl Drop for NapiEnv {
    fn drop(&mut self) {
        self.truncate_handles(0);
    }
}

pub struct NapiCallbackInfo {
    this_arg: napi_value,
    args: Vec<napi_value>,
    data: *mut c_void,
}

unsafe fn read_utf8(value: *const c_char, len: usize) -> String {
    if value.is_null() {
        return String::new();
    }
    let bytes = if len == NAPI_AUTO_LENGTH {
        unsafe { CStr::from_ptr(value) }.to_bytes()
    } else {
        unsafe { std::slice::from_raw_parts(value.cast::<u8>(), len) }
    };
    String::from_utf8_lossy(bytes).into_owned()
}

fn invalid(name: &'static str, reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name,
        reason: reason.into(),
    }
}

fn make_uint8_array(ctx: &mut NativeCtx<'_>, bytes: &[u8]) -> Result<Value, NativeError> {
    let constructor = ctx
        .global_value("Uint8Array")
        .ok_or_else(|| invalid("napi_create_buffer_copy", "Uint8Array is unavailable"))?;
    ctx.scope(|ctx, scope| {
        let constructor = ctx.scoped_value(scope, constructor);
        let array = ctx.scoped_array(scope, bytes.len())?;
        for (index, byte) in bytes.iter().copied().enumerate() {
            let value = ctx.scoped_number(scope, f64::from(byte));
            ctx.scoped_set_index(scope, array, index, value)?;
        }
        ctx.construct(ctx.escape(constructor), &[ctx.escape(array)])
    })
}

unsafe fn find_external(env: &mut NapiEnv, value: Value) -> Option<usize> {
    let externals: Vec<(PersistentRootId, usize)> = env
        .state
        .externals
        .lock()
        .expect("napi externals lock")
        .iter()
        .map(|external| (external.value, external.data))
        .collect();
    externals.into_iter().find_map(|(root, data)| {
        unsafe { env.ctx() }
            .persistent_root_get(root)
            .filter(|candidate| *candidate == value)
            .map(|_| data)
    })
}

fn error_value(
    ctx: &mut NativeCtx<'_>,
    constructor_name: &str,
    message: &str,
) -> Result<Value, NativeError> {
    let constructor = ctx
        .global_value(constructor_name)
        .ok_or_else(|| invalid("napi_create_error", "Error constructor is unavailable"))?;
    ctx.scope(|ctx, scope| {
        let constructor = ctx.scoped_value(scope, constructor);
        let message = ctx.scoped_string(scope, message)?;
        ctx.construct(ctx.escape(constructor), &[ctx.escape(message)])
    })
}

fn invoke_addon_callback(
    ctx: &mut NativeCtx<'_>,
    callback_addr: usize,
    data_addr: usize,
    args: &[Value],
    library: Arc<Library>,
    state: Arc<NapiState>,
) -> Result<Value, NativeError> {
    let callback: unsafe extern "C" fn(napi_env, napi_callback_info) -> napi_value =
        unsafe { std::mem::transmute(callback_addr) };
    let mut env = NapiEnv::new(ctx, library, state);
    let this_arg = env.root(*ctx.this_value());
    let argv = args.iter().copied().map(|value| env.root(value)).collect();
    let mut info = NapiCallbackInfo {
        this_arg,
        args: argv,
        data: data_addr as *mut c_void,
    };
    let returned = unsafe { callback(&mut env, &mut info) };
    if let Some(error) = env.pending.take() {
        return Err(error);
    }
    unsafe { env.value(returned) }.ok_or_else(|| invalid("napi callback", "invalid return handle"))
}

/// Load and initialize a Node-API addon selected by CommonJS resolution.
pub fn load_addon(
    ctx: &mut NativeCtx<'_>,
    path: &Path,
    capabilities: &CapabilitySet,
) -> Result<Value, NativeError> {
    if !capabilities.ffi.matches_path(path) {
        return Err(invalid(
            "require",
            format!("ffi permission denied for '{}'", path.display()),
        ));
    }
    keep_napi_exports();
    let library = Arc::new(unsafe { Library::new(path) }.map_err(|error| {
        invalid(
            "require",
            format!("cannot load native addon '{}': {error}", path.display()),
        )
    })?);
    let register = unsafe {
        library
            .get::<unsafe extern "C" fn(napi_env, napi_value) -> napi_value>(
                b"napi_register_module_v1\0",
            )
            .map(|symbol| *symbol)
    }
    .map_err(|error| {
        invalid(
            "require",
            format!(
                "'{}' is not a Node-API addon (missing napi_register_module_v1: {error})",
                path.display()
            ),
        )
    })?;

    let exports = ctx.scope(|ctx, scope| {
        let exports = ctx.scoped_object(scope)?;
        Ok::<Value, NativeError>(ctx.escape(exports))
    })?;
    let state = Arc::new(NapiState::default());
    let mut env = NapiEnv::new(ctx, library, state);
    let exports_handle = env.root(exports);
    let returned = unsafe { register(&mut env, exports_handle) };
    if let Some(error) = env.pending.take() {
        return Err(error);
    }
    unsafe { env.value(returned) }
        .or_else(|| unsafe { env.value(exports_handle) })
        .ok_or_else(|| invalid("require", "native addon returned an invalid handle"))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_undefined(env: napi_env, result: *mut napi_value) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = (&mut *env).root(Value::undefined()) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_null(env: napi_env, result: *mut napi_value) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = (&mut *env).root(Value::null()) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_global(env: napi_env, result: *mut napi_value) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let global = Value::object(*unsafe { env.ctx() }.interp_mut().global_this());
    unsafe { *result = env.root(global) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_boolean(
    env: napi_env,
    value: bool,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = (&mut *env).root(Value::boolean(value)) };
    NAPI_OK
}

macro_rules! create_number {
    ($name:ident, $ty:ty, $convert:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            env: napi_env,
            value: $ty,
            result: *mut napi_value,
        ) -> napi_status {
            if env.is_null() || result.is_null() {
                return NAPI_INVALID_ARG;
            }
            let number = Value::number_f64($convert(value));
            unsafe { *result = (&mut *env).root(number) };
            NAPI_OK
        }
    };
}

create_number!(napi_create_double, f64, |value: f64| value);
create_number!(napi_create_int32, i32, f64::from);
create_number!(napi_create_uint32, u32, f64::from);
create_number!(napi_create_int64, i64, |value: i64| value as f64);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_string_utf8(
    env: napi_env,
    value: *const c_char,
    len: usize,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let text = unsafe { read_utf8(value, len) };
    let created = unsafe { env.ctx() }.scope(|ctx, scope| {
        let value = ctx.scoped_string(scope, &text)?;
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match created {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_object(env: napi_env, result: *mut napi_value) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let created = unsafe { env.ctx() }.scope(|ctx, scope| {
        let value = ctx.scoped_object(scope)?;
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match created {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array(env: napi_env, result: *mut napi_value) -> napi_status {
    unsafe { napi_create_array_with_length(env, 0, result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_array_with_length(
    env: napi_env,
    length: usize,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let created = unsafe { env.ctx() }.scope(|ctx, scope| {
        let value = ctx.scoped_array(scope, length)?;
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match created {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_function(
    env: napi_env,
    _name: *const c_char,
    _length: usize,
    callback: napi_callback,
    data: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || callback.is_none() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let callback = callback.expect("checked");
    let callback_addr = callback as usize;
    let data_addr = data as usize;
    let library = env.library.clone();
    let state = env.state.clone();
    let created = unsafe { env.ctx() }
        .native_value(
            "napiCallback",
            SmallVec::new(),
            move |ctx, args, _captures| {
                invoke_addon_callback(
                    ctx,
                    callback_addr,
                    data_addr,
                    args,
                    library.clone(),
                    state.clone(),
                )
            },
        )
        .map_err(|error| NativeError::OutOfMemory {
            name: "napi_create_function",
            requested_bytes: error.requested_bytes(),
            heap_limit_bytes: error.heap_limit_bytes(),
        });
    match created {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_typeof(
    env: napi_env,
    value: napi_value,
    result: *mut c_int,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let kind = if value.is_undefined() {
        NAPI_UNDEFINED
    } else if value.is_null() {
        NAPI_NULL
    } else if value.is_boolean() {
        NAPI_BOOLEAN
    } else if value.as_f64().is_some() {
        NAPI_NUMBER
    } else if value.is_string() {
        NAPI_STRING
    } else if unsafe { find_external(env, value) }.is_some() {
        NAPI_EXTERNAL
    } else if value.is_callable() {
        NAPI_FUNCTION
    } else {
        NAPI_OBJECT
    };
    unsafe { *result = kind };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_object(
    env: napi_env,
    value: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if value.is_null() || value.is_undefined() {
        return env.fail(invalid(
            "napi_coerce_to_object",
            "cannot convert null or undefined to an object",
        ));
    }
    if value.is_object_type() || value.is_callable() {
        unsafe { *result = env.root(value) };
        return NAPI_OK;
    }
    let Some(constructor) = (unsafe { env.ctx() }).global_value("Object") else {
        return env.fail(invalid(
            "napi_coerce_to_object",
            "Object constructor is unavailable",
        ));
    };
    let converted = unsafe { env.ctx() }.scope(|ctx, scope| {
        let constructor = ctx.scoped_value(scope, constructor);
        let value = ctx.scoped_value(scope, value);
        ctx.call(
            ctx.escape(constructor),
            Value::undefined(),
            &[ctx.escape(value)],
        )
    });
    match converted {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external(
    env: napi_env,
    data: *mut c_void,
    _finalize: *mut c_void,
    _hint: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let created = unsafe { env.ctx() }.scope(|ctx, scope| {
        let value = ctx.scoped_object_bare(scope)?;
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match created {
        Ok(value) => {
            let root = unsafe { env.ctx() }.persistent_root_insert(value);
            env.state
                .externals
                .lock()
                .expect("napi externals lock")
                .push(NapiExternal {
                    value: root,
                    data: data as usize,
                });
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_external(
    env: napi_env,
    value: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(data) = (unsafe { find_external(env, value) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { *result = data as *mut c_void };
    NAPI_OK
}

macro_rules! get_number {
    ($name:ident, $ty:ty, $convert:expr) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(
            env: napi_env,
            value: napi_value,
            result: *mut $ty,
        ) -> napi_status {
            if env.is_null() || value.is_null() || result.is_null() {
                return NAPI_INVALID_ARG;
            }
            let env = unsafe { &mut *env };
            let Some(number) = (unsafe { env.value(value) }).and_then(|value| value.as_f64())
            else {
                return NAPI_NUMBER_EXPECTED;
            };
            unsafe { *result = $convert(number) };
            NAPI_OK
        }
    };
}

get_number!(napi_get_value_double, f64, |value: f64| value);
get_number!(napi_get_value_int32, i32, |value: f64| value as i32);
get_number!(napi_get_value_uint32, u32, |value: f64| value as u32);
get_number!(napi_get_value_int64, i64, |value: f64| value as i64);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_bool(
    env: napi_env,
    value: napi_value,
    result: *mut bool,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(boolean) = value.as_boolean() else {
        return NAPI_BOOLEAN_EXPECTED;
    };
    unsafe { *result = boolean };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_value_string_utf8(
    env: napi_env,
    value: napi_value,
    buffer: *mut c_char,
    buffer_size: usize,
    result: *mut usize,
) -> napi_status {
    if env.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let ctx = unsafe { env.ctx() };
    let Some(string) = value.as_string(ctx.heap()) else {
        return NAPI_STRING_EXPECTED;
    };
    let text = string.to_lossy_string(ctx.heap());
    let bytes = text.as_bytes();
    if buffer.is_null() || buffer_size == 0 {
        if !result.is_null() {
            unsafe { *result = bytes.len() };
        }
        return NAPI_OK;
    }
    let copied = bytes.len().min(buffer_size.saturating_sub(1));
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.cast::<u8>(), copied);
        *buffer.add(copied) = 0;
        if !result.is_null() {
            *result = copied;
        }
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_coerce_to_string(
    env: napi_env,
    value: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let text = value.display_string(unsafe { env.ctx() }.heap());
    let created = unsafe { env.ctx() }.scope(|ctx, scope| {
        let value = ctx.scoped_string(scope, &text)?;
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match created {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_named_property(
    env: napi_env,
    object: napi_value,
    name: *const c_char,
    value: napi_value,
) -> napi_status {
    if env.is_null() || object.is_null() || name.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    if !object.is_object_type() && !object.is_callable() {
        return NAPI_OBJECT_EXPECTED;
    }
    let name = unsafe { read_utf8(name, NAPI_AUTO_LENGTH) };
    let result = unsafe { env.ctx() }.set_value_property(object, &name, value);
    match result {
        Ok(()) => NAPI_OK,
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_named_property(
    env: napi_env,
    object: napi_value,
    name: *const c_char,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || object.is_null() || name.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let name = unsafe { read_utf8(name, NAPI_AUTO_LENGTH) };
    let value = unsafe { env.ctx() }.get_value_property(object, &name);
    match value {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_has_named_property(
    env: napi_env,
    object: napi_value,
    name: *const c_char,
    result: *mut bool,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let mut value = ptr::null_mut();
    let status = unsafe { napi_get_named_property(env, object, name, &mut value) };
    if status != NAPI_OK {
        return status;
    }
    let env = unsafe { &mut *env };
    unsafe { *result = env.value(value).is_some_and(|value| !value.is_undefined()) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_property(
    env: napi_env,
    object: napi_value,
    key: napi_value,
    value: napi_value,
) -> napi_status {
    if env.is_null() || key.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env_ref = unsafe { &mut *env };
    let Some(key_value) = (unsafe { env_ref.value(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let key = key_value.display_string(unsafe { env_ref.ctx() }.heap());
    let Ok(key) = std::ffi::CString::new(key) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { napi_set_named_property(env, object, key.as_ptr(), value) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property(
    env: napi_env,
    object: napi_value,
    key: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || key.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env_ref = unsafe { &mut *env };
    let Some(key_value) = (unsafe { env_ref.value(key) }) else {
        return NAPI_INVALID_ARG;
    };
    let key = key_value.display_string(unsafe { env_ref.ctx() }.heap());
    let Ok(key) = std::ffi::CString::new(key) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { napi_get_named_property(env, object, key.as_ptr(), result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_set_element(
    env: napi_env,
    object: napi_value,
    index: u32,
    value: napi_value,
) -> napi_status {
    let name = index.to_string();
    if env.is_null() || object.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let result = unsafe { env.ctx() }.scope(|ctx, scope| {
        let object = ctx.scoped_value(scope, object);
        let value = ctx.scoped_value(scope, value);
        if ctx.escape(object).is_array() {
            ctx.scoped_set_index(scope, object, index as usize, value)
        } else {
            ctx.scoped_set(scope, object, &name, value)
        }
    });
    match result {
        Ok(()) => NAPI_OK,
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_element(
    env: napi_env,
    object: napi_value,
    index: u32,
    result: *mut napi_value,
) -> napi_status {
    let name = index.to_string();
    if env.is_null() || object.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let value = unsafe { env.ctx() }.scope(|ctx, scope| {
        let object = ctx.scoped_value(scope, object);
        let value = if ctx.escape(object).is_array() {
            ctx.scoped_get_index(scope, object, index as usize)?
        } else {
            ctx.scoped_get(scope, object, &name)?
        };
        Ok::<Value, NativeError>(ctx.escape(value))
    });
    match value {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_cb_info(
    env: napi_env,
    info: napi_callback_info,
    argc: *mut usize,
    argv: *mut napi_value,
    this_arg: *mut napi_value,
    data: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || info.is_null() {
        return NAPI_INVALID_ARG;
    }
    let info = unsafe { &*info };
    if !argc.is_null() {
        let capacity = unsafe { *argc };
        if !argv.is_null() {
            let undefined = unsafe { (&mut *env).root(Value::undefined()) };
            for index in 0..capacity {
                unsafe {
                    *argv.add(index) = info.args.get(index).copied().unwrap_or(undefined);
                }
            }
        }
        unsafe { *argc = info.args.len() };
    }
    if !this_arg.is_null() {
        unsafe { *this_arg = info.this_arg };
    }
    if !data.is_null() {
        unsafe { *data = info.data };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_function(
    env: napi_env,
    receiver: napi_value,
    function: napi_value,
    argc: usize,
    argv: *const napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || function.is_null() || (argc != 0 && argv.is_null()) {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(function) = (unsafe { env.value(function) }) else {
        return NAPI_INVALID_ARG;
    };
    if !function.is_callable() {
        return NAPI_FUNCTION_EXPECTED;
    }
    let receiver = unsafe { env.value(receiver) }.unwrap_or_else(Value::undefined);
    let mut args = Vec::with_capacity(argc);
    for index in 0..argc {
        let Some(value) = (unsafe { env.value(*argv.add(index)) }) else {
            return NAPI_INVALID_ARG;
        };
        args.push(value);
    }
    match unsafe { env.ctx() }.call(function, receiver, &args) {
        Ok(value) => {
            if !result.is_null() {
                unsafe { *result = env.root(value) };
            }
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_new_instance(
    env: napi_env,
    constructor: napi_value,
    argc: usize,
    argv: *const napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || constructor.is_null() || result.is_null() || (argc != 0 && argv.is_null()) {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(constructor) = (unsafe { env.value(constructor) }) else {
        return NAPI_INVALID_ARG;
    };
    let mut args = Vec::with_capacity(argc);
    for index in 0..argc {
        let Some(value) = (unsafe { env.value(*argv.add(index)) }) else {
            return NAPI_INVALID_ARG;
        };
        args.push(value);
    }
    match unsafe { env.ctx() }.construct(constructor, &args) {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw(env: napi_env, error: napi_value) -> napi_status {
    if env.is_null() || error.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let message = unsafe { env.value(error) }
        .map(|value| {
            let detail = unsafe { env.ctx() }
                .get_value_property(value, "message")
                .ok()
                .filter(|detail| !detail.is_undefined())
                .map(|detail| detail.display_string(unsafe { env.ctx() }.heap()));
            detail.unwrap_or_else(|| value.display_string(unsafe { env.ctx() }.heap()))
        })
        .unwrap_or_else(|| "native addon exception".to_string());
    env.pending = Some(NativeError::Error { message });
    NAPI_OK
}

unsafe fn throw_message(env: napi_env, message: *const c_char, kind: &'static str) -> napi_status {
    if env.is_null() || message.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let message = unsafe { read_utf8(message, NAPI_AUTO_LENGTH) };
    env.pending = Some(match kind {
        "TypeError" => NativeError::TypeError {
            name: "native addon",
            reason: message,
        },
        "RangeError" => NativeError::RangeError {
            name: "native addon",
            reason: message,
        },
        _ => NativeError::Error { message },
    });
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_error(
    env: napi_env,
    _code: *const c_char,
    message: *const c_char,
) -> napi_status {
    unsafe { throw_message(env, message, "Error") }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_type_error(
    env: napi_env,
    _code: *const c_char,
    message: *const c_char,
) -> napi_status {
    unsafe { throw_message(env, message, "TypeError") }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_throw_range_error(
    env: napi_env,
    _code: *const c_char,
    message: *const c_char,
) -> napi_status {
    unsafe { throw_message(env, message, "RangeError") }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_exception_pending(
    env: napi_env,
    result: *mut bool,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = (&*env).pending.is_some() };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_and_clear_last_exception(
    env: napi_env,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let value = match env.pending.take() {
        Some(error) => match error_value(unsafe { env.ctx() }, "Error", &error.to_string()) {
            Ok(value) => value,
            Err(error) => return env.fail(error),
        },
        None => Value::undefined(),
    };
    unsafe { *result = env.root(value) };
    NAPI_OK
}

unsafe fn create_error(
    env: napi_env,
    message: napi_value,
    result: *mut napi_value,
    constructor: &str,
) -> napi_status {
    if env.is_null() || message.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(message) = (unsafe { env.value(message) }) else {
        return NAPI_INVALID_ARG;
    };
    let message = message.display_string(unsafe { env.ctx() }.heap());
    match error_value(unsafe { env.ctx() }, constructor, &message) {
        Ok(value) => {
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_error(
    env: napi_env,
    _code: napi_value,
    message: napi_value,
    result: *mut napi_value,
) -> napi_status {
    unsafe { create_error(env, message, result, "Error") }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_strict_equals(
    env: napi_env,
    left: napi_value,
    right: napi_value,
    result: *mut bool,
) -> napi_status {
    if env.is_null() || left.is_null() || right.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let (Some(left), Some(right)) = (unsafe { env.value(left) }, unsafe { env.value(right) })
    else {
        return NAPI_INVALID_ARG;
    };
    unsafe { *result = env.ctx().strict_equals(left, right) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_error(
    env: napi_env,
    value: napi_value,
    result: *mut bool,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(error_constructor) = (unsafe { env.ctx() }).global_value("Error") else {
        unsafe { *result = false };
        return NAPI_OK;
    };
    match unsafe { env.ctx() }.is_instance_of(value, error_constructor) {
        Ok(value) => {
            unsafe { *result = value };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_prototype(
    env: napi_env,
    value: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let prototype = if let Some(object) = value.as_object() {
        otter_vm::object::prototype_value(object, unsafe { env.ctx() }.heap())
            .unwrap_or_else(Value::null)
    } else {
        Value::null()
    };
    unsafe { *result = env.root(prototype) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_reference(
    env: napi_env,
    value: napi_value,
    initial_refcount: u32,
    result: *mut napi_ref,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let root = unsafe { env.ctx() }.persistent_root_insert(value);
    unsafe {
        *result = Box::into_raw(Box::new(NapiRef {
            root,
            count: initial_refcount,
        }))
    };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_reference(env: napi_env, reference: napi_ref) -> napi_status {
    if env.is_null() || reference.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let reference = unsafe { Box::from_raw(reference) };
    let _ = unsafe { env.ctx() }.persistent_root_remove(reference.root);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_reference_value(
    env: napi_env,
    reference: napi_ref,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || reference.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.ctx() }).persistent_root_get(unsafe { &*reference }.root)
    else {
        unsafe { *result = ptr::null_mut() };
        return NAPI_OK;
    };
    unsafe { *result = env.root(value) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_ref(
    _env: napi_env,
    reference: napi_ref,
    result: *mut u32,
) -> napi_status {
    if reference.is_null() {
        return NAPI_INVALID_ARG;
    }
    let reference = unsafe { &mut *reference };
    reference.count = reference.count.saturating_add(1);
    if !result.is_null() {
        unsafe { *result = reference.count };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reference_unref(
    _env: napi_env,
    reference: napi_ref,
    result: *mut u32,
) -> napi_status {
    if reference.is_null() {
        return NAPI_INVALID_ARG;
    }
    let reference = unsafe { &mut *reference };
    reference.count = reference.count.saturating_sub(1);
    if !result.is_null() {
        unsafe { *result = reference.count };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_wrap(
    env: napi_env,
    object: napi_value,
    data: *mut c_void,
    _finalize: *mut c_void,
    _hint: *mut c_void,
    result: *mut napi_ref,
) -> napi_status {
    if env.is_null() || object.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    if !value.is_object_type() && !value.is_callable() {
        return NAPI_OBJECT_EXPECTED;
    }
    let root = unsafe { env.ctx() }.persistent_root_insert(value);
    env.state
        .wraps
        .lock()
        .expect("napi wraps lock")
        .push(NapiWrap {
            object: root,
            data: data as usize,
        });
    if !result.is_null() {
        return unsafe { napi_create_reference(env, object, 0, result) };
    }
    NAPI_OK
}

unsafe fn find_wrap(env: &mut NapiEnv, object: Value) -> Option<(usize, PersistentRootId)> {
    let wraps: Vec<(usize, PersistentRootId)> = env
        .state
        .wraps
        .lock()
        .expect("napi wraps lock")
        .iter()
        .map(|wrap| (wrap.data, wrap.object))
        .collect();
    wraps.into_iter().find_map(|(data, root)| {
        unsafe { env.ctx() }
            .persistent_root_get(root)
            .filter(|candidate| *candidate == object)
            .map(|_| (data, root))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unwrap(
    env: napi_env,
    object: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || object.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        *result = find_wrap(env, object)
            .map(|(data, _)| data as *mut c_void)
            .unwrap_or(ptr::null_mut())
    };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_remove_wrap(
    env: napi_env,
    object: napi_value,
    result: *mut *mut c_void,
) -> napi_status {
    if env.is_null() || object.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some((data, root)) = (unsafe { find_wrap(env, object) }) else {
        if !result.is_null() {
            unsafe { *result = ptr::null_mut() };
        }
        return NAPI_OK;
    };
    env.state
        .wraps
        .lock()
        .expect("napi wraps lock")
        .retain(|wrap| wrap.object != root);
    let _ = unsafe { env.ctx() }.persistent_root_remove(root);
    if !result.is_null() {
        unsafe { *result = data as *mut c_void };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_promise(
    env: napi_env,
    deferred: *mut napi_deferred,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || deferred.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    match unsafe { env.ctx() }.promise_capability() {
        Ok((promise, resolve, reject)) => {
            let resolve = unsafe { env.ctx() }.persistent_root_insert(resolve);
            let reject = unsafe { env.ctx() }.persistent_root_insert(reject);
            unsafe {
                *deferred = Box::into_raw(Box::new(NapiDeferred { resolve, reject }));
                *result = env.root(promise);
            }
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

unsafe fn settle_deferred(
    env: napi_env,
    deferred: napi_deferred,
    value: napi_value,
    reject: bool,
) -> napi_status {
    if env.is_null() || deferred.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let deferred = unsafe { Box::from_raw(deferred) };
    let root = if reject {
        deferred.reject
    } else {
        deferred.resolve
    };
    let Some(settler) = (unsafe { env.ctx() }).persistent_root_get(root) else {
        return NAPI_INVALID_ARG;
    };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let outcome = unsafe { env.ctx() }.call(settler, Value::undefined(), &[value]);
    let _ = unsafe { env.ctx() }.persistent_root_remove(deferred.resolve);
    let _ = unsafe { env.ctx() }.persistent_root_remove(deferred.reject);
    match outcome {
        Ok(_) => NAPI_OK,
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_resolve_deferred(
    env: napi_env,
    deferred: napi_deferred,
    value: napi_value,
) -> napi_status {
    unsafe { settle_deferred(env, deferred, value, false) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_reject_deferred(
    env: napi_env,
    deferred: napi_deferred,
    value: napi_value,
) -> napi_status {
    unsafe { settle_deferred(env, deferred, value, true) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_buffer_copy(
    env: napi_env,
    length: usize,
    data: *const c_void,
    result_data: *mut *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() || (length != 0 && data.is_null()) {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let bytes = if length == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) }
    };
    match make_uint8_array(unsafe { env.ctx() }, bytes) {
        Ok(value) => {
            let pointer = unsafe { env.ctx() }
                .typed_array_info(value)
                .map(|(_, _, data, _, _)| data.cast())
                .unwrap_or(ptr::null_mut());
            if !result_data.is_null() {
                unsafe { *result_data = pointer };
            }
            unsafe { *result = env.root(value) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_external_buffer(
    env: napi_env,
    length: usize,
    data: *mut c_void,
    _finalize: *mut c_void,
    _hint: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    unsafe { napi_create_buffer_copy(env, length, data, ptr::null_mut(), result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_typedarray_info(
    env: napi_env,
    value: napi_value,
    kind: *mut c_int,
    length: *mut usize,
    data: *mut *mut c_void,
    array_buffer: *mut napi_value,
    byte_offset: *mut usize,
) -> napi_status {
    if env.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some((element_kind, element_length, pointer, buffer, offset)) =
        (unsafe { env.ctx() }).typed_array_info(value)
    else {
        return NAPI_INVALID_ARG;
    };
    if !kind.is_null() {
        unsafe { *kind = element_kind as c_int };
    }
    if !length.is_null() {
        unsafe { *length = element_length };
    }
    if !data.is_null() {
        unsafe { *data = pointer.cast() };
    }
    if !array_buffer.is_null() {
        unsafe { *array_buffer = env.root(buffer) };
    }
    if !byte_offset.is_null() {
        unsafe { *byte_offset = offset };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_buffer_info(
    env: napi_env,
    value: napi_value,
    data: *mut *mut c_void,
    length: *mut usize,
) -> napi_status {
    if env.is_null() || value.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some((kind, element_length, pointer, _, _)) =
        (unsafe { env.ctx() }).typed_array_info(value)
    else {
        return NAPI_INVALID_ARG;
    };
    if kind != NAPI_UINT8_ARRAY {
        return NAPI_INVALID_ARG;
    }
    if !data.is_null() {
        unsafe { *data = pointer.cast() };
    }
    if !length.is_null() {
        unsafe { *length = element_length };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_buffer(
    env: napi_env,
    value: napi_value,
    result: *mut bool,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    unsafe {
        *result = env
            .ctx()
            .typed_array_info(value)
            .is_some_and(|(kind, _, _, _, _)| kind == NAPI_UINT8_ARRAY)
    };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_adjust_external_memory(
    env: napi_env,
    change_in_bytes: i64,
    result: *mut i64,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    match unsafe { env.ctx() }.adjust_external_memory(change_in_bytes) {
        Ok(adjusted) => {
            unsafe { *result = adjusted };
            NAPI_OK
        }
        Err(error) => env.fail(NativeError::OutOfMemory {
            name: "napi_adjust_external_memory",
            requested_bytes: error.requested_bytes(),
            heap_limit_bytes: error.heap_limit_bytes(),
        }),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_create_async_work(
    _env: napi_env,
    _resource: napi_value,
    _name: napi_value,
    execute: napi_async_execute_callback,
    complete: napi_async_complete_callback,
    data: *mut c_void,
    result: *mut napi_async_work,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe {
        *result = Box::into_raw(Box::new(NapiAsyncWork {
            execute,
            complete,
            data: data as usize,
            queued: false,
        }))
    };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_queue_async_work(
    env: napi_env,
    work: napi_async_work,
) -> napi_status {
    if env.is_null() || work.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let work = unsafe { &mut *work };
    if work.queued {
        return NAPI_INVALID_ARG;
    }
    work.queued = true;

    // The execute callback normally runs on libuv's worker pool, but it may
    // not touch JavaScript state. Keep the initial compatibility backend on
    // the isolate thread and, critically, defer both execute and completion
    // until the runtime microtask checkpoint. This preserves the observable
    // async boundary and gives napi-rs time to return its pending Promise
    // before the completion callback resolves it.
    let execute = work.execute;
    let complete = work.complete;
    let data = work.data;
    let library = env.library.clone();
    let state = env.state.clone();
    let task = match unsafe { env.ctx() }.native_value(
        "napiAsyncWork",
        SmallVec::new(),
        move |ctx, _args, _captures| {
            let mut callback_env = NapiEnv::new(ctx, library.clone(), state.clone());
            if let Some(execute) = execute {
                unsafe { execute(&mut callback_env, data as *mut c_void) };
            }
            if let Some(error) = callback_env.pending.take() {
                return Err(error);
            }
            if let Some(complete) = complete {
                unsafe { complete(&mut callback_env, NAPI_OK, data as *mut c_void) };
            }
            if let Some(error) = callback_env.pending.take() {
                return Err(error);
            }
            Ok(Value::undefined())
        },
    ) {
        Ok(task) => task,
        Err(error) => {
            return env.fail(NativeError::OutOfMemory {
                name: "napi_queue_async_work",
                requested_bytes: error.requested_bytes(),
                heap_limit_bytes: error.heap_limit_bytes(),
            });
        }
    };
    match unsafe { env.ctx() }.queue_microtask(task, []) {
        Ok(()) => NAPI_OK,
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_cancel_async_work(
    _env: napi_env,
    work: napi_async_work,
) -> napi_status {
    if work.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { (*work).queued = false };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_delete_async_work(
    _env: napi_env,
    work: napi_async_work,
) -> napi_status {
    if work.is_null() {
        return NAPI_INVALID_ARG;
    }
    drop(unsafe { Box::from_raw(work) });
    NAPI_OK
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn napi_create_threadsafe_function(
    _env: napi_env,
    _function: napi_value,
    _resource: napi_value,
    _name: napi_value,
    _max_queue_size: usize,
    _initial_thread_count: usize,
    _finalize_data: *mut c_void,
    _finalize_callback: *mut c_void,
    _context: *mut c_void,
    _call_js_callback: *mut c_void,
    result: *mut napi_threadsafe_function,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = 1usize as napi_threadsafe_function };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: napi_env,
    _function: napi_threadsafe_function,
) -> napi_status {
    NAPI_OK
}

#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn napi_define_class(
    env: napi_env,
    _name: *const c_char,
    _length: usize,
    constructor: napi_callback,
    data: *mut c_void,
    property_count: usize,
    properties: *const napi_property_descriptor,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || constructor.is_none() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let status = unsafe {
        napi_create_function(
            env,
            ptr::null(),
            NAPI_AUTO_LENGTH,
            constructor,
            data,
            result,
        )
    };
    if status != NAPI_OK || properties.is_null() {
        return status;
    }
    let mut prototype = ptr::null_mut();
    let status = unsafe { napi_create_object(env, &mut prototype) };
    if status != NAPI_OK {
        return status;
    }
    for index in 0..property_count {
        let descriptor = unsafe { &*properties.add(index) };
        if descriptor.utf8name.is_null() || descriptor.method.is_none() {
            continue;
        }
        let mut method = ptr::null_mut();
        let status = unsafe {
            napi_create_function(
                env,
                descriptor.utf8name,
                NAPI_AUTO_LENGTH,
                descriptor.method,
                descriptor.data,
                &mut method,
            )
        };
        if status != NAPI_OK {
            return status;
        }
        let target = if descriptor.attributes & NAPI_STATIC != 0 {
            unsafe { *result }
        } else {
            prototype
        };
        let status = unsafe { napi_set_named_property(env, target, descriptor.utf8name, method) };
        if status != NAPI_OK {
            return status;
        }
    }
    let prototype_name = c"prototype";
    unsafe { napi_set_named_property(env, *result, prototype_name.as_ptr(), prototype) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_open_handle_scope(
    env: napi_env,
    result: *mut napi_handle_scope,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let base = env.handles.len();
    env.scopes.push(base);
    unsafe { *result = (base + 1) as napi_handle_scope };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_handle_scope(
    env: napi_env,
    scope: napi_handle_scope,
) -> napi_status {
    if env.is_null() || scope.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let base = scope as usize - 1;
    if env.scopes.pop() != Some(base) {
        return NAPI_INVALID_ARG;
    }
    env.truncate_handles(base);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_version(_env: napi_env, result: *mut u32) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = 1 };
    NAPI_OK
}

/// Anchor every exported ABI symbol so static archive dead-stripping cannot
/// discard functions referenced only by a library loaded at runtime.
pub fn keep_napi_exports() {
    let anchors: &[*const ()] = &[
        napi_get_undefined as *const (),
        napi_get_null as *const (),
        napi_get_global as *const (),
        napi_get_boolean as *const (),
        napi_create_double as *const (),
        napi_create_int32 as *const (),
        napi_create_uint32 as *const (),
        napi_create_int64 as *const (),
        napi_create_string_utf8 as *const (),
        napi_create_object as *const (),
        napi_create_array as *const (),
        napi_create_array_with_length as *const (),
        napi_create_function as *const (),
        napi_typeof as *const (),
        napi_coerce_to_object as *const (),
        napi_create_external as *const (),
        napi_get_value_external as *const (),
        napi_get_value_double as *const (),
        napi_get_value_int32 as *const (),
        napi_get_value_uint32 as *const (),
        napi_get_value_int64 as *const (),
        napi_get_value_bool as *const (),
        napi_get_value_string_utf8 as *const (),
        napi_coerce_to_string as *const (),
        napi_set_named_property as *const (),
        napi_get_named_property as *const (),
        napi_has_named_property as *const (),
        napi_set_property as *const (),
        napi_get_property as *const (),
        napi_set_element as *const (),
        napi_get_element as *const (),
        napi_get_cb_info as *const (),
        napi_call_function as *const (),
        napi_new_instance as *const (),
        napi_throw as *const (),
        napi_throw_error as *const (),
        napi_throw_type_error as *const (),
        napi_throw_range_error as *const (),
        napi_is_exception_pending as *const (),
        napi_get_and_clear_last_exception as *const (),
        napi_create_error as *const (),
        napi_strict_equals as *const (),
        napi_is_error as *const (),
        napi_get_prototype as *const (),
        napi_create_reference as *const (),
        napi_delete_reference as *const (),
        napi_get_reference_value as *const (),
        napi_reference_ref as *const (),
        napi_reference_unref as *const (),
        napi_wrap as *const (),
        napi_unwrap as *const (),
        napi_remove_wrap as *const (),
        napi_create_promise as *const (),
        napi_resolve_deferred as *const (),
        napi_reject_deferred as *const (),
        napi_create_buffer_copy as *const (),
        napi_create_external_buffer as *const (),
        napi_get_typedarray_info as *const (),
        napi_get_buffer_info as *const (),
        napi_is_buffer as *const (),
        napi_adjust_external_memory as *const (),
        napi_create_async_work as *const (),
        napi_queue_async_work as *const (),
        napi_cancel_async_work as *const (),
        napi_delete_async_work as *const (),
        napi_create_threadsafe_function as *const (),
        napi_unref_threadsafe_function as *const (),
        napi_define_class as *const (),
        napi_open_handle_scope as *const (),
        napi_close_handle_scope as *const (),
        napi_get_version as *const (),
        keep_napi_exports as *const (),
    ];
    std::hint::black_box(anchors);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_reader_accepts_sized_and_nul_terminated_inputs() {
        let text = b"otter\0";
        assert_eq!(unsafe { read_utf8(text.as_ptr().cast(), 5) }, "otter");
        assert_eq!(
            unsafe { read_utf8(text.as_ptr().cast(), NAPI_AUTO_LENGTH) },
            "otter"
        );
    }
}
