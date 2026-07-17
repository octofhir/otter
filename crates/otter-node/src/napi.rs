//! Node-API host ABI for loading native `.node` addons.
//!
//! A native addon is a dynamic library whose symbol-based initializer or
//! constructor-based `napi_module_register` callback calls the `napi_*` C
//! symbols exported by the Otter executable. This module supplies that
//! VM-neutral ABI directly; it does not embed Node or V8 and does not route
//! through `napi-rs` (which is an addon-side binding).
//!
//! # Contents
//! - [`load_addon`] opens a capability-approved library and runs registration.
//! - [`NapiEnv`] owns stable C handles backed by Otter persistent roots.
//! - Exported `napi_*` functions implement the initial Node-API value,
//!   property, callback, exception, external-memory, buffer, Promise,
//!   async-work, thread-safe-function, property-descriptor, and handle-scope
//!   surface.
//!
//! # Invariants
//! - Both `read` and `ffi` capabilities are checked before native code loads.
//! - A `napi_value` never stores a raw moving-heap offset. It points to a stable
//!   Rust box containing a persistent-root id; every access rereads the root.
//! - `napi_env` has addon lifetime, while its mutator context is installed only
//!   for the current isolate turn. Addons may retain the environment pointer,
//!   but worker threads may only use APIs documented as thread-safe.
//! - VM allocations and mutations use the `NativeScope` / `Local` API.
//! - The raw context pointer is confined to the synchronous C ABI turn.
//! - Async execute/completion callbacks receive the stable addon environment
//!   at the runtime checkpoint; no VM context crosses a thread boundary.
//! - Thread-safe function calls cross the owned runtime inbox and reacquire
//!   persistent-rooted callbacks on the isolate thread.
//! - Loaded code stays mapped while any JS callback or async task created from
//!   it is alive.
//!   Unsupported ABI symbols remain absent, so the platform loader fails fast.
//! - Deprecated constructor registration is captured in a thread-local frame
//!   only while the platform loader is mapping that addon.
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

use std::cell::RefCell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use libloading::Library;
use otter_runtime::{
    CapabilitySet, RuntimeExecutionContext, RuntimeKeepAlive, RuntimeLiveness, RuntimeTask,
    RuntimeTaskSpawner,
};
use otter_vm::{Local, NativeCall, NativeCtx, NativeError, NativeScope, PersistentRootId, Value};

pub type napi_env = *mut NapiEnv;
pub type napi_value = *mut c_void;
pub type napi_callback_info = *mut NapiCallbackInfo;
pub type napi_status = c_int;
pub type napi_handle_scope = *mut c_void;
pub type napi_escapable_handle_scope = *mut NapiEscapableHandleScope;
pub type napi_ref = *mut NapiRef;
pub type napi_deferred = *mut NapiDeferred;
pub type napi_async_work = *mut NapiAsyncWork;
pub type napi_threadsafe_function = *mut c_void;
pub type napi_callback = Option<unsafe extern "C" fn(napi_env, napi_callback_info) -> napi_value>;
pub type napi_async_execute_callback = Option<unsafe extern "C" fn(napi_env, *mut c_void)>;
pub type napi_async_complete_callback =
    Option<unsafe extern "C" fn(napi_env, napi_status, *mut c_void)>;
type napi_addon_register_func = Option<unsafe extern "C" fn(napi_env, napi_value) -> napi_value>;

const NAPI_OK: napi_status = 0;
const NAPI_INVALID_ARG: napi_status = 1;
const NAPI_OBJECT_EXPECTED: napi_status = 2;
const NAPI_STRING_EXPECTED: napi_status = 3;
const NAPI_FUNCTION_EXPECTED: napi_status = 5;
const NAPI_NUMBER_EXPECTED: napi_status = 6;
const NAPI_BOOLEAN_EXPECTED: napi_status = 7;
const NAPI_GENERIC_FAILURE: napi_status = 9;
const NAPI_PENDING_EXCEPTION: napi_status = 10;
const NAPI_ESCAPE_CALLED_TWICE: napi_status = 12;
const NAPI_QUEUE_FULL: napi_status = 15;
const NAPI_CLOSING: napi_status = 16;

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
const NAPI_WRITABLE: c_int = 1;
const NAPI_ENUMERABLE: c_int = 1 << 1;
const NAPI_CONFIGURABLE: c_int = 1 << 2;
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

#[repr(C)]
struct NapiModule {
    nm_version: c_int,
    nm_flags: u32,
    nm_filename: *const c_char,
    nm_register_func: napi_addon_register_func,
    nm_modname: *const c_char,
    nm_priv: *mut c_void,
    reserved: [*mut c_void; 4],
}

#[repr(C)]
pub struct NapiExtendedErrorInfo {
    error_message: *const c_char,
    engine_reserved: *mut c_void,
    engine_error_code: u32,
    error_code: napi_status,
}

pub struct NapiEscapableHandleScope {
    base: usize,
    escaped: Option<Box<NapiHandle>>,
}

thread_local! {
    /// One registration slot per nested `Library::new` call on this thread.
    static LEGACY_MODULE_REGISTRATIONS: RefCell<Vec<Option<usize>>> = const {
        RefCell::new(Vec::new())
    };
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

struct NapiThreadsafeFunction {
    inner: Arc<NapiThreadsafeFunctionInner>,
}

struct NapiThreadsafeFunctionInner {
    state: Arc<NapiState>,
    task_spawner: RuntimeTaskSpawner,
    execution_context: RuntimeExecutionContext,
    function_root: Option<PersistentRootId>,
    call_js_callback: usize,
    thread_count: AtomicUsize,
    queued: AtomicUsize,
    max_queue_size: usize,
    closing: AtomicBool,
    finalize_data: usize,
    finalize_callback: usize,
    context: usize,
    keep_alive: RuntimeKeepAlive,
}

struct NapiThreadsafeFunctionCallTask {
    function: Arc<NapiThreadsafeFunctionInner>,
    data: usize,
}

impl RuntimeTask for NapiThreadsafeFunctionCallTask {
    fn run(
        self: Box<Self>,
        runtime: &mut otter_runtime::Runtime,
    ) -> Result<(), otter_runtime::OtterError> {
        self.function.queued.fetch_sub(1, Ordering::AcqRel);
        let function = self.function.clone();
        let execution_context = function.execution_context.clone();
        let data = self.data;
        runtime
            .run_native_event(&execution_context, move |ctx| {
                let callback_value = function
                    .function_root
                    .and_then(|root| ctx.persistent_root_get(root));
                if function.call_js_callback == 0 {
                    if let Some(callback) = callback_value {
                        ctx.call(callback, Value::undefined(), &[])?;
                    }
                    return Ok(Value::undefined());
                }
                let callback: unsafe extern "C" fn(napi_env, napi_value, *mut c_void, *mut c_void) =
                    unsafe { std::mem::transmute(function.call_js_callback) };
                with_stable_env(ctx, &function.state, |env| {
                    let callback_value = callback_value
                        .map(|value| env.root(value))
                        .unwrap_or(ptr::null_mut());
                    unsafe {
                        callback(
                            env,
                            callback_value,
                            function.context as *mut c_void,
                            data as *mut c_void,
                        )
                    };
                    if let Some(error) = env.pending.take() {
                        return Err(error);
                    }
                    Ok(Value::undefined())
                })
            })
            .map(|_| ())
    }
}

struct NapiThreadsafeFunctionFinalizeTask {
    function: Arc<NapiThreadsafeFunctionInner>,
}

impl RuntimeTask for NapiThreadsafeFunctionFinalizeTask {
    fn run(
        self: Box<Self>,
        runtime: &mut otter_runtime::Runtime,
    ) -> Result<(), otter_runtime::OtterError> {
        let function = self.function.clone();
        let execution_context = function.execution_context.clone();
        let result = runtime
            .run_native_event(&execution_context, move |ctx| {
                if let Some(root) = function.function_root {
                    let _ = ctx.persistent_root_remove(root);
                }
                if function.finalize_callback != 0 {
                    let callback: unsafe extern "C" fn(napi_env, *mut c_void, *mut c_void) =
                        unsafe { std::mem::transmute(function.finalize_callback) };
                    with_stable_env(ctx, &function.state, |env| {
                        unsafe {
                            callback(
                                env,
                                function.finalize_data as *mut c_void,
                                function.context as *mut c_void,
                            )
                        };
                        if let Some(error) = env.pending.take() {
                            return Err(error);
                        }
                        Ok::<(), NativeError>(())
                    })?;
                }
                Ok(Value::undefined())
            })
            .map(|_| ());
        self.function.keep_alive.close();
        result
    }
}

struct NapiState {
    library: Arc<Library>,
    env_ptr: AtomicUsize,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    execution_context: Option<RuntimeExecutionContext>,
    wraps: Mutex<Vec<NapiWrap>>,
    externals: Mutex<Vec<NapiExternal>>,
    cleanup_hooks: Mutex<Vec<NapiCleanupHook>>,
    finalizers: Mutex<Vec<NapiFinalizer>>,
}

impl NapiState {
    fn new(
        library: Arc<Library>,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
        execution_context: Option<RuntimeExecutionContext>,
    ) -> Self {
        Self {
            library,
            env_ptr: AtomicUsize::new(0),
            runtime_task_spawner,
            execution_context,
            wraps: Mutex::new(Vec::new()),
            externals: Mutex::new(Vec::new()),
            cleanup_hooks: Mutex::new(Vec::new()),
            finalizers: Mutex::new(Vec::new()),
        }
    }
}

impl Drop for NapiState {
    fn drop(&mut self) {
        let _keep_library_loaded = &self.library;
        for hook in self
            .cleanup_hooks
            .get_mut()
            .expect("napi cleanup hooks lock")
            .drain(..)
            .rev()
        {
            let callback: unsafe extern "C" fn(*mut c_void) =
                unsafe { std::mem::transmute(hook.callback) };
            unsafe { callback(hook.data as *mut c_void) };
        }
        for finalizer in self
            .finalizers
            .get_mut()
            .expect("napi finalizers lock")
            .drain(..)
            .rev()
        {
            let callback: unsafe extern "C" fn(napi_env, *mut c_void, *mut c_void) =
                unsafe { std::mem::transmute(finalizer.callback) };
            unsafe {
                callback(
                    ptr::null_mut(),
                    finalizer.data as *mut c_void,
                    finalizer.hint as *mut c_void,
                )
            };
        }
        let env = self.env_ptr.swap(0, Ordering::AcqRel);
        if env != 0 {
            drop(unsafe { Box::from_raw(env as *mut NapiEnv) });
        }
    }
}

struct NapiCleanupHook {
    callback: usize,
    data: usize,
}

struct NapiFinalizer {
    object_root: PersistentRootId,
    callback: usize,
    data: usize,
    hint: usize,
    remove_with_wrap: bool,
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
    last_error: NapiExtendedErrorInfo,
    state: Weak<NapiState>,
}

impl NapiEnv {
    fn new(state: Weak<NapiState>) -> Self {
        Self {
            ctx: ptr::null_mut(),
            handles: Vec::new(),
            scopes: Vec::new(),
            pending: None,
            last_error: NapiExtendedErrorInfo {
                error_message: c"Otter Node-API call failed".as_ptr(),
                engine_reserved: ptr::null_mut(),
                engine_error_code: 0,
                error_code: NAPI_GENERIC_FAILURE,
            },
            state,
        }
    }

    /// Recover the mutator-turn context hidden behind the C ABI.
    ///
    /// # Safety
    /// `self.ctx` is live only while the synchronous registration/callback
    /// frame that installed it remains on the Rust stack.
    unsafe fn ctx(&mut self) -> &mut NativeCtx<'static> {
        assert!(!self.ctx.is_null(), "napi_env used outside an isolate turn");
        unsafe { &mut *self.ctx.cast::<NativeCtx<'static>>() }
    }

    fn state(&self) -> Arc<NapiState> {
        self.state
            .upgrade()
            .expect("Node-API state outlives its stable environment")
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
        self.last_error.error_code = NAPI_PENDING_EXCEPTION;
        NAPI_PENDING_EXCEPTION
    }

    fn truncate_handles(&mut self, base: usize) {
        while self.handles.len() > base {
            let handle = self.handles.pop().expect("length checked");
            let _ = unsafe { self.ctx() }.persistent_root_remove(handle.root);
        }
    }
}

fn install_stable_env(state: &Arc<NapiState>) {
    let env = Box::new(NapiEnv::new(Arc::downgrade(state)));
    let previous = state
        .env_ptr
        .swap(Box::into_raw(env) as usize, Ordering::AcqRel);
    assert_eq!(previous, 0, "stable napi_env installed once");
}

fn with_stable_env<R>(
    ctx: &mut NativeCtx<'_>,
    state: &Arc<NapiState>,
    run: impl FnOnce(&mut NapiEnv) -> R,
) -> R {
    let env_ptr = state.env_ptr.load(Ordering::Acquire) as *mut NapiEnv;
    assert!(!env_ptr.is_null(), "stable napi_env is installed");
    let env = unsafe { &mut *env_ptr };
    let previous_ctx = std::mem::replace(&mut env.ctx, (ctx as *mut NativeCtx<'_>).cast());
    let handle_base = env.handles.len();
    let result = run(env);
    env.truncate_handles(handle_base);
    env.ctx = previous_ctx;
    result
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
    ctx.scope(|mut scope| {
        let constructor = scope.value(constructor);
        let array = scope.array(bytes.len())?;
        for (index, byte) in bytes.iter().copied().enumerate() {
            let value = scope.number(f64::from(byte));
            scope.set_index(array, index, value)?;
        }
        let result = scope.construct(constructor, &[array])?;
        Ok(scope.finish(result))
    })
}

fn make_uint8_array_len(ctx: &mut NativeCtx<'_>, length: usize) -> Result<Value, NativeError> {
    let constructor = ctx
        .global_value("Uint8Array")
        .ok_or_else(|| invalid("napi_create_buffer", "Uint8Array is unavailable"))?;
    ctx.scope(|mut scope| {
        let constructor = scope.value(constructor);
        let length = scope.number(length as f64);
        let result = scope.construct(constructor, &[length])?;
        Ok(scope.finish(result))
    })
}

unsafe fn find_external(env: &mut NapiEnv, value: Value) -> Option<usize> {
    let state = env.state();
    let externals: Vec<(PersistentRootId, usize)> = state
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
    ctx.scope(|mut scope| {
        let constructor = scope.value(constructor);
        let message = scope.string(message)?;
        let result = scope.construct(constructor, &[message])?;
        Ok(scope.finish(result))
    })
}

fn invoke_addon_callback(
    ctx: &mut NativeCtx<'_>,
    callback_addr: usize,
    data_addr: usize,
    args: &[Value],
    state: Arc<NapiState>,
) -> Result<Value, NativeError> {
    let callback: unsafe extern "C" fn(napi_env, napi_callback_info) -> napi_value =
        unsafe { std::mem::transmute(callback_addr) };
    let this_value = *ctx.this_value();
    with_stable_env(ctx, &state, |env| {
        let this_arg = env.root(this_value);
        let argv = args.iter().copied().map(|value| env.root(value)).collect();
        let mut info = NapiCallbackInfo {
            this_arg,
            args: argv,
            data: data_addr as *mut c_void,
        };
        let returned = unsafe { callback(env, &mut info) };
        if let Some(error) = env.pending.take() {
            return Err(error);
        }
        unsafe { env.value(returned) }
            .ok_or_else(|| invalid("napi callback", "invalid return handle"))
    })
}

/// Load and initialize a Node-API addon selected by CommonJS resolution.
pub fn load_addon<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    path: &Path,
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    if !capabilities.ffi.matches_path(path) {
        return Err(invalid(
            "require",
            format!("ffi permission denied for '{}'", path.display()),
        ));
    }
    keep_napi_exports();
    LEGACY_MODULE_REGISTRATIONS.with(|registrations| registrations.borrow_mut().push(None));
    let loaded = unsafe { Library::new(path) };
    let legacy_register = LEGACY_MODULE_REGISTRATIONS.with(|registrations| {
        registrations
            .borrow_mut()
            .pop()
            .expect("legacy Node-API registration frame")
    });
    let library = Arc::new(loaded.map_err(|error| {
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
    .ok()
    .or_else(|| {
        legacy_register.map(|address| unsafe {
            std::mem::transmute::<usize, unsafe extern "C" fn(napi_env, napi_value) -> napi_value>(
                address,
            )
        })
    })
    .ok_or_else(|| {
        invalid(
            "require",
            format!(
                "'{}' is not a Node-API addon (no supported registration entry point)",
                path.display()
            ),
        )
    })?;

    let exports = scope.object()?;
    let register_addon = |ctx: &mut NativeCtx<'_>, exports: Value| {
        let state = Arc::new(NapiState::new(
            library.clone(),
            runtime_task_spawner,
            ctx.execution_context().cloned(),
        ));
        install_stable_env(&state);
        with_stable_env(ctx, &state, |env| {
            // Persist the rooted scope input before invoking addon code: C
            // registration may allocate and trigger a moving collection.
            let exports_handle = env.root(exports);
            let returned = unsafe { register(env, exports_handle) };
            if let Some(error) = env.pending.take() {
                return Err(error);
            }
            unsafe { env.value(returned) }
                .or_else(|| unsafe { env.value(exports_handle) })
                .ok_or_else(|| invalid("require", "native addon returned an invalid handle"))
        })
    };
    // SAFETY: the callback's first operation involving `exports` is
    // `env.root(exports)`, which installs a persistent traced handle before
    // addon registration can allocate or collect. The returned value is then
    // reread from one of those persistent handles after registration's final
    // allocation. Neither raw value nor the temporary context escapes this
    // synchronous registration turn.
    unsafe { scope.with_ffi_value(exports, register_addon) }
}

/// Capture the deprecated constructor-based registration callback while the
/// platform loader is mapping an addon on the current thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_module_register(module: *mut c_void) {
    if module.is_null() {
        return;
    }
    let Some(register) = (unsafe { &*module.cast::<NapiModule>() }).nm_register_func else {
        return;
    };
    LEGACY_MODULE_REGISTRATIONS.with(|registrations| {
        if let Some(slot) = registrations.borrow_mut().last_mut() {
            *slot = Some(register as usize);
        }
    });
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
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.string(&text)?;
        Ok::<Value, NativeError>(scope.finish(value))
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
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.object()?;
        Ok::<Value, NativeError>(scope.finish(value))
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
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.array(length)?;
        Ok::<Value, NativeError>(scope.finish(value))
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
    let state = env.state();
    let call = NativeCall::Dynamic(Arc::new(move |ctx, args, _captures| {
        invoke_addon_callback(ctx, callback_addr, data_addr, args, state.clone())
    }));
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.native_call("napiCallback", 0, call)?;
        Ok::<Value, NativeError>(scope.finish(value))
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
    let converted = unsafe { env.ctx() }.scope(|mut scope| {
        let constructor = scope.value(constructor);
        let value = scope.value(value);
        let this_value = scope.undefined();
        let result = scope.call(constructor, this_value, &[value])?;
        Ok(scope.finish(result))
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
    finalize: *mut c_void,
    hint: *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.bare_object()?;
        Ok::<Value, NativeError>(scope.finish(value))
    });
    match created {
        Ok(value) => {
            let root = unsafe { env.ctx() }.persistent_root_insert(value);
            let state = env.state();
            state
                .externals
                .lock()
                .expect("napi externals lock")
                .push(NapiExternal {
                    value: root,
                    data: data as usize,
                });
            if !finalize.is_null() {
                state
                    .finalizers
                    .lock()
                    .expect("napi finalizers lock")
                    .push(NapiFinalizer {
                        object_root: root,
                        callback: finalize as usize,
                        data: data as usize,
                        hint: hint as usize,
                        remove_with_wrap: false,
                    });
            }
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
    let created = unsafe { env.ctx() }.scope(|mut scope| {
        let value = scope.string(&text)?;
        Ok::<Value, NativeError>(scope.finish(value))
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
pub unsafe extern "C" fn napi_has_property(
    env: napi_env,
    object: napi_value,
    key: napi_value,
    result: *mut bool,
) -> napi_status {
    if result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let mut value = ptr::null_mut();
    let status = unsafe { napi_get_property(env, object, key, &mut value) };
    if status != NAPI_OK {
        return status;
    }
    let env = unsafe { &mut *env };
    unsafe { *result = env.value(value).is_some_and(|value| !value.is_undefined()) };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_define_properties(
    env: napi_env,
    object: napi_value,
    property_count: usize,
    properties: *const napi_property_descriptor,
) -> napi_status {
    if env.is_null() || object.is_null() || (property_count != 0 && properties.is_null()) {
        return NAPI_INVALID_ARG;
    }
    for index in 0..property_count {
        let descriptor = unsafe { &*properties.add(index) };
        let name = if !descriptor.utf8name.is_null() {
            descriptor.utf8name
        } else if !descriptor.name.is_null() {
            let env = unsafe { &mut *env };
            let Some(name) = (unsafe { env.value(descriptor.name) }) else {
                return NAPI_INVALID_ARG;
            };
            let Ok(name) = std::ffi::CString::new(name.display_string(unsafe { env.ctx() }.heap()))
            else {
                return NAPI_INVALID_ARG;
            };
            let status = unsafe { define_one_property(env, object, name.as_ptr(), descriptor) };
            if status != NAPI_OK {
                return status;
            }
            continue;
        } else {
            return NAPI_INVALID_ARG;
        };
        let status = unsafe { define_one_property(env, object, name, descriptor) };
        if status != NAPI_OK {
            return status;
        }
    }
    NAPI_OK
}

unsafe fn define_one_property(
    env: napi_env,
    object: napi_value,
    name: *const c_char,
    descriptor: &napi_property_descriptor,
) -> napi_status {
    let name_text = unsafe { read_utf8(name, NAPI_AUTO_LENGTH) };
    let mut property_value = descriptor.value;
    if descriptor.method.is_some() {
        let status = unsafe {
            napi_create_function(
                env,
                name,
                NAPI_AUTO_LENGTH,
                descriptor.method,
                descriptor.data,
                &mut property_value,
            )
        };
        if status != NAPI_OK {
            return status;
        }
    }
    if !property_value.is_null() {
        let env = unsafe { &mut *env };
        let (Some(object), Some(value)) = (unsafe { env.value(object) }, unsafe {
            env.value(property_value)
        }) else {
            return NAPI_INVALID_ARG;
        };
        let flags = otter_vm::object::PropertyFlags::new(
            descriptor.attributes & NAPI_WRITABLE != 0,
            descriptor.attributes & NAPI_ENUMERABLE != 0,
            descriptor.attributes & NAPI_CONFIGURABLE != 0,
        );
        let defined = unsafe { env.ctx() }.scope(|mut scope| {
            let object = scope.value(object);
            let value = scope.value(value);
            scope.define(object, &name_text, value, flags)
        });
        return match defined {
            Ok(()) => NAPI_OK,
            Err(error) => env.fail(error),
        };
    }

    let mut getter = ptr::null_mut();
    if descriptor.getter.is_some() {
        let status = unsafe {
            napi_create_function(
                env,
                name,
                NAPI_AUTO_LENGTH,
                descriptor.getter,
                descriptor.data,
                &mut getter,
            )
        };
        if status != NAPI_OK {
            return status;
        }
    }
    let mut setter = ptr::null_mut();
    if descriptor.setter.is_some() {
        let status = unsafe {
            napi_create_function(
                env,
                name,
                NAPI_AUTO_LENGTH,
                descriptor.setter,
                descriptor.data,
                &mut setter,
            )
        };
        if status != NAPI_OK {
            return status;
        }
    }
    if getter.is_null() && setter.is_null() {
        return NAPI_INVALID_ARG;
    }

    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    let getter = unsafe { env.value(getter) };
    let setter = unsafe { env.value(setter) };
    let Some(object_constructor) = (unsafe { env.ctx() }).global_value("Object") else {
        return NAPI_GENERIC_FAILURE;
    };
    let defined = unsafe { env.ctx() }.scope(|mut scope| {
        let object = scope.value(object);
        let object_constructor = scope.value(object_constructor);
        let descriptor_object = scope.object()?;
        let name = scope.string(&name_text)?;
        if let Some(getter) = getter {
            let getter = scope.value(getter);
            scope.set(descriptor_object, "get", getter)?;
        }
        if let Some(setter) = setter {
            let setter = scope.value(setter);
            scope.set(descriptor_object, "set", setter)?;
        }
        let enumerable = scope.boolean(descriptor.attributes & NAPI_ENUMERABLE != 0);
        let configurable = scope.boolean(descriptor.attributes & NAPI_CONFIGURABLE != 0);
        scope.set(descriptor_object, "enumerable", enumerable)?;
        scope.set(descriptor_object, "configurable", configurable)?;
        let define_property = scope.get(object_constructor, "defineProperty")?;
        let _ = scope.call(
            define_property,
            object_constructor,
            &[object, name, descriptor_object],
        )?;
        Ok::<(), NativeError>(())
    });
    match defined {
        Ok(()) => NAPI_OK,
        Err(error) => env.fail(error),
    }
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
    let result = unsafe { env.ctx() }.scope(|mut scope| {
        let object = scope.value(object);
        let value = scope.value(value);
        if scope.is_array(object)? {
            scope.set_index(object, index as usize, value)
        } else {
            scope.set(object, &name, value)
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
    let value = unsafe { env.ctx() }.scope(|mut scope| {
        let object = scope.value(object);
        let value = if scope.is_array(object)? {
            scope.index(object, index as usize)?
        } else {
            scope.get(object, &name)?
        };
        Ok::<Value, NativeError>(scope.finish(value))
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
pub unsafe extern "C" fn napi_create_type_error(
    env: napi_env,
    _code: napi_value,
    message: napi_value,
    result: *mut napi_value,
) -> napi_status {
    unsafe { create_error(env, message, result, "TypeError") }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_last_error_info(
    env: napi_env,
    result: *mut *const NapiExtendedErrorInfo,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { *result = &(&*env).last_error };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_exception(env: napi_env, error: napi_value) -> napi_status {
    unsafe { napi_throw(env, error) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_fatal_error(
    _location: *const c_char,
    _location_len: usize,
    _message: *const c_char,
    _message_len: usize,
) -> ! {
    std::process::abort()
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
    finalize: *mut c_void,
    hint: *mut c_void,
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
    let state = env.state();
    state.wraps.lock().expect("napi wraps lock").push(NapiWrap {
        object: root,
        data: data as usize,
    });
    if !finalize.is_null() {
        state
            .finalizers
            .lock()
            .expect("napi finalizers lock")
            .push(NapiFinalizer {
                object_root: root,
                callback: finalize as usize,
                data: data as usize,
                hint: hint as usize,
                remove_with_wrap: true,
            });
    }
    if !result.is_null() {
        return unsafe { napi_create_reference(env, object, 0, result) };
    }
    NAPI_OK
}

unsafe fn find_wrap(env: &mut NapiEnv, object: Value) -> Option<(usize, PersistentRootId)> {
    let state = env.state();
    let wraps: Vec<(usize, PersistentRootId)> = state
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
    let state = env.state();
    state
        .wraps
        .lock()
        .expect("napi wraps lock")
        .retain(|wrap| wrap.object != root);
    state
        .finalizers
        .lock()
        .expect("napi finalizers lock")
        .retain(|finalizer| !(finalizer.remove_with_wrap && finalizer.object_root == root));
    let _ = unsafe { env.ctx() }.persistent_root_remove(root);
    if !result.is_null() {
        unsafe { *result = data as *mut c_void };
    }
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_env_cleanup_hook(
    env: napi_env,
    callback: *mut c_void,
    data: *mut c_void,
) -> napi_status {
    if env.is_null() || callback.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { &mut *env }
        .state()
        .cleanup_hooks
        .lock()
        .expect("napi cleanup hooks lock")
        .push(NapiCleanupHook {
            callback: callback as usize,
            data: data as usize,
        });
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_add_finalizer(
    env: napi_env,
    object: napi_value,
    data: *mut c_void,
    callback: *mut c_void,
    hint: *mut c_void,
    result: *mut napi_ref,
) -> napi_status {
    if env.is_null() || object.is_null() || callback.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object_value) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    if !object_value.is_object_type() && !object_value.is_callable() {
        return NAPI_OBJECT_EXPECTED;
    }
    let root = unsafe { env.ctx() }.persistent_root_insert(object_value);
    env.state()
        .finalizers
        .lock()
        .expect("napi finalizers lock")
        .push(NapiFinalizer {
            object_root: root,
            callback: callback as usize,
            data: data as usize,
            hint: hint as usize,
            remove_with_wrap: false,
        });
    if !result.is_null() {
        return unsafe { napi_create_reference(env, object, 0, result) };
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
pub unsafe extern "C" fn napi_create_buffer(
    env: napi_env,
    length: usize,
    result_data: *mut *mut c_void,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    match make_uint8_array_len(unsafe { env.ctx() }, length) {
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
pub unsafe extern "C" fn napi_is_array(
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
    match unsafe { env.ctx() }.is_array(value) {
        Ok(is_array) => {
            unsafe { *result = is_array };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_promise(
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
    unsafe { *result = value.is_promise() };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_is_typedarray(
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
    unsafe { *result = value.is_typed_array() };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_array_length(
    env: napi_env,
    value: napi_value,
    result: *mut u32,
) -> napi_status {
    if env.is_null() || value.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(value) = (unsafe { env.value(value) }) else {
        return NAPI_INVALID_ARG;
    };
    let Some(length) = (unsafe { env.ctx() }).array_length(value) else {
        return NAPI_INVALID_ARG;
    };
    unsafe { *result = length.min(u32::MAX as usize) as u32 };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_property_names(
    env: napi_env,
    object: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || object.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let Some(object) = (unsafe { env.value(object) }) else {
        return NAPI_INVALID_ARG;
    };
    if !object.is_object_type() && !object.is_callable() {
        return NAPI_OBJECT_EXPECTED;
    }
    let Some(object_constructor) = (unsafe { env.ctx() }).global_value("Object") else {
        return env.fail(invalid(
            "napi_get_property_names",
            "Object constructor is unavailable",
        ));
    };
    let names = unsafe { env.ctx() }.scope(|mut scope| {
        let constructor = scope.value(object_constructor);
        let object = scope.value(object);
        let keys = scope.get(constructor, "keys")?;
        let names = scope.call(keys, constructor, &[object])?;
        Ok(scope.finish(names))
    });
    match names {
        Ok(names) => {
            unsafe { *result = env.root(names) };
            NAPI_OK
        }
        Err(error) => env.fail(error),
    }
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
    let state = env.state();
    let call = NativeCall::Dynamic(Arc::new(move |ctx, _args, _captures| {
        with_stable_env(ctx, &state, |callback_env| {
            if let Some(execute) = execute {
                unsafe { execute(callback_env, data as *mut c_void) };
            }
            if let Some(error) = callback_env.pending.take() {
                return Err(error);
            }
            if let Some(complete) = complete {
                unsafe { complete(callback_env, NAPI_OK, data as *mut c_void) };
            }
            if let Some(error) = callback_env.pending.take() {
                return Err(error);
            }
            Ok(Value::undefined())
        })
    }));
    let task = match unsafe { env.ctx() }.scope(|mut scope| {
        let task = scope.native_call("napiAsyncWork", 0, call)?;
        Ok::<Value, NativeError>(scope.finish(task))
    }) {
        Ok(task) => task,
        Err(error) => return env.fail(error),
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
    env: napi_env,
    function: napi_value,
    _resource: napi_value,
    _name: napi_value,
    max_queue_size: usize,
    initial_thread_count: usize,
    finalize_data: *mut c_void,
    finalize_callback: *mut c_void,
    context: *mut c_void,
    call_js_callback: *mut c_void,
    result: *mut napi_threadsafe_function,
) -> napi_status {
    if env.is_null() || result.is_null() || initial_thread_count == 0 {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    if function.is_null() && call_js_callback.is_null() {
        return NAPI_INVALID_ARG;
    }
    let state = env.state();
    let (Some(task_spawner), Some(execution_context)) = (
        state.runtime_task_spawner.clone(),
        state.execution_context.clone(),
    ) else {
        return NAPI_GENERIC_FAILURE;
    };
    let function_root = if function.is_null() {
        None
    } else {
        let Some(function) = (unsafe { env.value(function) }) else {
            return NAPI_INVALID_ARG;
        };
        if !function.is_callable() {
            return NAPI_FUNCTION_EXPECTED;
        }
        Some(unsafe { env.ctx() }.persistent_root_insert(function))
    };
    let keep_alive = task_spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let function = Box::new(NapiThreadsafeFunction {
        inner: Arc::new(NapiThreadsafeFunctionInner {
            state,
            task_spawner,
            execution_context,
            function_root,
            call_js_callback: call_js_callback as usize,
            thread_count: AtomicUsize::new(initial_thread_count),
            queued: AtomicUsize::new(0),
            max_queue_size,
            closing: AtomicBool::new(false),
            finalize_data: finalize_data as usize,
            finalize_callback: finalize_callback as usize,
            context: context as usize,
            keep_alive,
        }),
    });
    unsafe { *result = Box::into_raw(function).cast() };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_call_threadsafe_function(
    function: napi_threadsafe_function,
    data: *mut c_void,
    _mode: c_int,
) -> napi_status {
    if function.is_null() {
        return NAPI_INVALID_ARG;
    }
    let function = unsafe { &*function.cast::<NapiThreadsafeFunction>() }
        .inner
        .clone();
    if function.closing.load(Ordering::Acquire) {
        return NAPI_CLOSING;
    }
    if function.max_queue_size != 0 {
        let reserved =
            function
                .queued
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                    (queued < function.max_queue_size).then_some(queued + 1)
                });
        if reserved.is_err() {
            return NAPI_QUEUE_FULL;
        }
    } else {
        function.queued.fetch_add(1, Ordering::AcqRel);
    }
    let task = NapiThreadsafeFunctionCallTask {
        function: function.clone(),
        data: data as usize,
    };
    match function.task_spawner.enqueue(task, RuntimeLiveness::Unref) {
        Ok(()) => NAPI_OK,
        Err(_) => {
            function.queued.fetch_sub(1, Ordering::AcqRel);
            if function.closing.load(Ordering::Acquire) {
                NAPI_CLOSING
            } else {
                NAPI_GENERIC_FAILURE
            }
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_acquire_threadsafe_function(
    function: napi_threadsafe_function,
) -> napi_status {
    if function.is_null() {
        return NAPI_INVALID_ARG;
    }
    let function = &unsafe { &*function.cast::<NapiThreadsafeFunction>() }.inner;
    if function.closing.load(Ordering::Acquire) {
        return NAPI_CLOSING;
    }
    function.thread_count.fetch_add(1, Ordering::AcqRel);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_get_threadsafe_function_context(
    function: napi_threadsafe_function,
    result: *mut *mut c_void,
) -> napi_status {
    if function.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let function = &unsafe { &*function.cast::<NapiThreadsafeFunction>() }.inner;
    unsafe { *result = function.context as *mut c_void };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_release_threadsafe_function(
    function: napi_threadsafe_function,
    mode: c_int,
) -> napi_status {
    if function.is_null() {
        return NAPI_INVALID_ARG;
    }
    let function_ref = &unsafe { &*function.cast::<NapiThreadsafeFunction>() }.inner;
    if mode == 1 {
        function_ref.closing.store(true, Ordering::Release);
    }
    let previous = function_ref.thread_count.fetch_sub(1, Ordering::AcqRel);
    if previous == 0 {
        function_ref.thread_count.store(0, Ordering::Release);
        return NAPI_INVALID_ARG;
    }
    if previous != 1 {
        return NAPI_OK;
    }
    function_ref.closing.store(true, Ordering::Release);
    let function = unsafe { Box::from_raw(function.cast::<NapiThreadsafeFunction>()) };
    let task = NapiThreadsafeFunctionFinalizeTask {
        function: function.inner.clone(),
    };
    match function
        .inner
        .task_spawner
        .enqueue(task, RuntimeLiveness::Unref)
    {
        Ok(()) => NAPI_OK,
        Err(_) => {
            function.inner.keep_alive.close();
            NAPI_GENERIC_FAILURE
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_unref_threadsafe_function(
    _env: napi_env,
    function: napi_threadsafe_function,
) -> napi_status {
    if function.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { &*function.cast::<NapiThreadsafeFunction>() }
        .inner
        .keep_alive
        .unref();
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_ref_threadsafe_function(
    _env: napi_env,
    function: napi_threadsafe_function,
) -> napi_status {
    if function.is_null() {
        return NAPI_INVALID_ARG;
    }
    unsafe { &*function.cast::<NapiThreadsafeFunction>() }
        .inner
        .keep_alive
        .ref_();
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
pub unsafe extern "C" fn napi_open_escapable_handle_scope(
    env: napi_env,
    result: *mut napi_escapable_handle_scope,
) -> napi_status {
    if env.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let base = env.handles.len();
    env.scopes.push(base);
    unsafe {
        *result = Box::into_raw(Box::new(NapiEscapableHandleScope {
            base,
            escaped: None,
        }))
    };
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_escape_handle(
    env: napi_env,
    scope: napi_escapable_handle_scope,
    escapee: napi_value,
    result: *mut napi_value,
) -> napi_status {
    if env.is_null() || scope.is_null() || escapee.is_null() || result.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let scope = unsafe { &mut *scope };
    if env.scopes.last() != Some(&scope.base) {
        return NAPI_INVALID_ARG;
    }
    if scope.escaped.is_some() {
        return NAPI_ESCAPE_CALLED_TWICE;
    }
    let Some(value) = (unsafe { env.value(escapee) }) else {
        return NAPI_INVALID_ARG;
    };
    let root = unsafe { env.ctx() }.persistent_root_insert(value);
    let handle = Box::new(NapiHandle { root });
    unsafe { *result = (&*handle as *const NapiHandle).cast_mut().cast() };
    scope.escaped = Some(handle);
    NAPI_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn napi_close_escapable_handle_scope(
    env: napi_env,
    scope: napi_escapable_handle_scope,
) -> napi_status {
    if env.is_null() || scope.is_null() {
        return NAPI_INVALID_ARG;
    }
    let env = unsafe { &mut *env };
    let mut scope = unsafe { Box::from_raw(scope) };
    if env.scopes.pop() != Some(scope.base) {
        let _ = Box::into_raw(scope);
        return NAPI_INVALID_ARG;
    }
    env.truncate_handles(scope.base);
    if let Some(handle) = scope.escaped.take() {
        env.handles.push(handle);
    }
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
        napi_module_register as *const (),
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
        napi_has_property as *const (),
        napi_define_properties as *const (),
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
        napi_create_type_error as *const (),
        napi_get_last_error_info as *const (),
        napi_fatal_exception as *const (),
        napi_fatal_error as *const (),
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
        napi_add_env_cleanup_hook as *const (),
        napi_add_finalizer as *const (),
        napi_create_promise as *const (),
        napi_resolve_deferred as *const (),
        napi_reject_deferred as *const (),
        napi_create_buffer_copy as *const (),
        napi_create_buffer as *const (),
        napi_create_external_buffer as *const (),
        napi_get_typedarray_info as *const (),
        napi_get_buffer_info as *const (),
        napi_is_buffer as *const (),
        napi_is_array as *const (),
        napi_is_promise as *const (),
        napi_is_typedarray as *const (),
        napi_get_array_length as *const (),
        napi_get_property_names as *const (),
        napi_adjust_external_memory as *const (),
        napi_create_async_work as *const (),
        napi_queue_async_work as *const (),
        napi_cancel_async_work as *const (),
        napi_delete_async_work as *const (),
        napi_create_threadsafe_function as *const (),
        napi_call_threadsafe_function as *const (),
        napi_acquire_threadsafe_function as *const (),
        napi_get_threadsafe_function_context as *const (),
        napi_release_threadsafe_function as *const (),
        napi_unref_threadsafe_function as *const (),
        napi_ref_threadsafe_function as *const (),
        napi_define_class as *const (),
        napi_open_handle_scope as *const (),
        napi_close_handle_scope as *const (),
        napi_open_escapable_handle_scope as *const (),
        napi_escape_handle as *const (),
        napi_close_escapable_handle_scope as *const (),
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
