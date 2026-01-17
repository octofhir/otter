//! Safe extension API for registering host functions.

use crate::apis::json_to_js_value;
use crate::bindings::*;
use crate::error::{JscError, JscResult};
use crate::value::js_string_to_rust;
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::debug;

pub type OpResult = JscResult<serde_json::Value>;
pub type OpFuture = Pin<Box<dyn Future<Output = OpResult> + Send + 'static>>;
/// Type alias for extension initialization functions.
pub type ExtensionInitFn = Arc<dyn Fn(&ExtensionState) + Send + Sync>;

#[derive(Clone)]
pub struct ExtensionState {
    inner: Arc<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

impl Default for ExtensionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn put<T: Any + Send + Sync>(&self, value: T) {
        let mut map = self.inner.lock();
        map.insert(TypeId::of::<T>(), Arc::new(value));
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let map = self.inner.lock();
        map.get(&TypeId::of::<T>()).and_then(|value| {
            let value = value.clone();
            value.downcast::<T>().ok()
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RuntimeContextHandle(usize);

impl RuntimeContextHandle {
    pub fn new(ctx: JSContextRef) -> Self {
        Self(ctx as usize)
    }

    pub fn ctx(self) -> JSContextRef {
        self.0 as JSContextRef
    }
}

#[derive(Clone)]
pub struct OpContext {
    state: ExtensionState,
}

impl OpContext {
    pub fn state(&self) -> ExtensionState {
        self.state.clone()
    }
}

#[derive(Clone)]
pub enum OpHandler {
    Sync(Arc<dyn Fn(OpContext, Vec<serde_json::Value>) -> OpResult + Send + Sync>),
    Async(Arc<dyn Fn(OpContext, Vec<serde_json::Value>) -> OpFuture + Send + Sync>),
}

#[derive(Clone)]
pub struct OpDecl {
    name: String,
    handler: OpHandler,
}

impl OpDecl {
    pub fn name(&self) -> &str {
        &self.name
    }
}

pub fn op_sync<F>(name: &str, handler: F) -> OpDecl
where
    F: Fn(OpContext, Vec<serde_json::Value>) -> OpResult + Send + Sync + 'static,
{
    OpDecl {
        name: name.to_string(),
        handler: OpHandler::Sync(Arc::new(handler)),
    }
}

pub fn op_async<F, Fut>(name: &str, handler: F) -> OpDecl
where
    F: Fn(OpContext, Vec<serde_json::Value>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = OpResult> + Send + 'static,
{
    OpDecl {
        name: name.to_string(),
        handler: OpHandler::Async(Arc::new(move |ctx, args| Box::pin(handler(ctx, args)))),
    }
}

#[derive(Clone)]
pub struct Extension {
    name: String,
    ops: Vec<OpDecl>,
    init: Option<ExtensionInitFn>,
    /// JavaScript code to execute after registering ops.
    /// This is useful for setting up wrapper functions that use the ops.
    js_code: Option<String>,
}

impl Extension {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ops: Vec::new(),
            init: None,
            js_code: None,
        }
    }

    /// Get the name of this extension
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn with_ops(mut self, ops: Vec<OpDecl>) -> Self {
        self.ops = ops;
        self
    }

    pub fn with_init<F>(mut self, init: F) -> Self
    where
        F: Fn(&ExtensionState) + Send + Sync + 'static,
    {
        self.init = Some(Arc::new(init));
        self
    }

    /// Add JavaScript code to be executed after ops are registered.
    /// This code has access to all registered ops as global functions.
    pub fn with_js(mut self, js_code: &str) -> Self {
        self.js_code = Some(js_code.to_string());
        self
    }

    /// Get the JavaScript code for this extension.
    pub fn js_code(&self) -> Option<&str> {
        self.js_code.as_deref()
    }
}

pub struct ExtensionRegistry {
    ops: Mutex<HashMap<String, OpDecl>>,
    state: ExtensionState,
    pending_promises: Mutex<HashMap<u64, PendingPromise>>,
    pending_rx: Receiver<AsyncOpMessage>,
    next_promise_id: AtomicU64,
    async_queue: Arc<AsyncQueue>,
}

#[derive(Debug)]
struct PendingPromise {
    resolve: JSObjectRef,
    reject: JSObjectRef,
}

struct AsyncOpMessage {
    id: u64,
    result: OpResult,
}

struct AsyncQueue {
    pending_tx: Sender<AsyncOpMessage>,
    inflight_ops: AtomicU64,
}

impl AsyncQueue {
    fn new(sender: Sender<AsyncOpMessage>) -> Self {
        Self {
            pending_tx: sender,
            inflight_ops: AtomicU64::new(0),
        }
    }

    fn queue_result(&self, promise_id: u64, result: OpResult) {
        let _ = self.pending_tx.send(AsyncOpMessage {
            id: promise_id,
            result,
        });
    }

    fn on_result(&self) {
        self.inflight_ops.fetch_sub(1, Ordering::Relaxed);
    }

    fn inflight(&self) -> u64 {
        self.inflight_ops.load(Ordering::Relaxed)
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        let (pending_tx, pending_rx) = unbounded();
        Self {
            ops: Mutex::new(HashMap::new()),
            state: ExtensionState::new(),
            pending_promises: Mutex::new(HashMap::new()),
            pending_rx,
            next_promise_id: AtomicU64::new(1),
            async_queue: Arc::new(AsyncQueue::new(pending_tx)),
        }
    }

    pub fn state(&self) -> ExtensionState {
        self.state.clone()
    }

    pub fn register_extension(&self, extension: Extension, ctx: JSContextRef) -> JscResult<()> {
        debug!(
            extension = extension.name(),
            ops_count = extension.ops.len(),
            "Registering extension"
        );

        self.state.put(RuntimeContextHandle::new(ctx));

        if let Some(init) = extension.init.as_ref() {
            init(&self.state);
        }

        for op in extension.ops.clone() {
            self.register_op(op, ctx)?;
        }

        // Execute JavaScript code if provided
        if let Some(js_code) = extension.js_code() {
            self.execute_js(ctx, js_code, &format!("<{}>", extension.name()))?;
        }

        debug!(
            extension = extension.name(),
            "Extension registered successfully"
        );
        Ok(())
    }

    fn execute_js(&self, ctx: JSContextRef, code: &str, source: &str) -> JscResult<()> {
        let code_cstr = CString::new(code).map_err(|e| JscError::internal(e.to_string()))?;
        let source_cstr = CString::new(source).map_err(|e| JscError::internal(e.to_string()))?;

        unsafe {
            let code_ref = JSStringCreateWithUTF8CString(code_cstr.as_ptr());
            let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());
            let mut exception: JSValueRef = std::ptr::null_mut();

            let _result = JSEvaluateScript(
                ctx,
                code_ref,
                std::ptr::null_mut(),
                source_ref,
                1,
                &mut exception,
            );

            JSStringRelease(code_ref);
            JSStringRelease(source_ref);

            if !exception.is_null() {
                // Try to get error message
                let exc_str = JSValueToStringCopy(ctx, exception, std::ptr::null_mut());
                if !exc_str.is_null() {
                    let msg = js_string_to_rust(exc_str);
                    JSStringRelease(exc_str);
                    return Err(JscError::internal(format!(
                        "Extension JS init failed: {}",
                        msg
                    )));
                }
                return Err(JscError::internal("Extension JS init failed".to_string()));
            }
        }

        Ok(())
    }

    pub fn poll_promises(&self, ctx: JSContextRef) -> JscResult<usize> {
        let mut resolved = 0;
        for message in self.pending_rx.try_iter() {
            let promise = {
                let mut pending = self.pending_promises.lock();
                pending.remove(&message.id)
            };

            let Some(promise) = promise else {
                self.async_queue.on_result();
                continue;
            };

            let result = unsafe { resolve_pending(ctx, promise, message.result) };
            if result.is_ok() {
                resolved += 1;
            }
            self.async_queue.on_result();
        }

        Ok(resolved)
    }

    pub fn has_pending_async_ops(&self) -> bool {
        if self.async_queue.inflight() > 0 {
            return true;
        }
        if !self.pending_rx.is_empty() {
            return true;
        }
        !self.pending_promises.lock().is_empty()
    }

    fn create_deferred_promise(&self, ctx: JSContextRef) -> JscResult<(JSValueRef, u64)> {
        let mut resolve: JSObjectRef = std::ptr::null_mut();
        let mut reject: JSObjectRef = std::ptr::null_mut();
        let mut exception: JSValueRef = std::ptr::null_mut();

        let promise =
            unsafe { JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exception) };

        if !exception.is_null() || promise.is_null() || resolve.is_null() || reject.is_null() {
            return Err(JscError::internal(
                "Failed to create deferred promise".to_string(),
            ));
        }

        unsafe {
            JSValueProtect(ctx, resolve as JSValueRef);
            JSValueProtect(ctx, reject as JSValueRef);
        }

        let id = self.next_promise_id.fetch_add(1, Ordering::Relaxed);
        let mut pending = self.pending_promises.lock();
        pending.insert(id, PendingPromise { resolve, reject });

        Ok((promise as JSValueRef, id))
    }

    fn register_op(&self, op: OpDecl, ctx: JSContextRef) -> JscResult<()> {
        let mut ops = self.ops.lock();
        if ops.contains_key(op.name()) {
            return Err(JscError::internal(format!(
                "Op already registered: {}",
                op.name()
            )));
        }

        register_js_op(ctx, op.name(), self as *const ExtensionRegistry as usize)?;
        ops.insert(op.name().to_string(), op);
        Ok(())
    }

    fn get_op(&self, name: &str) -> Option<OpDecl> {
        let ops = self.ops.lock();
        ops.get(name).cloned()
    }
}

thread_local! {
    static REGISTRY_MAP: RefCell<HashMap<usize, Arc<ExtensionRegistry>>> =
        RefCell::new(HashMap::new());

    /// Thread-local Tokio runtime handle for async operations in worker threads.
    static TOKIO_HANDLE: RefCell<Option<tokio::runtime::Handle>> = const { RefCell::new(None) };
}

fn registry_key(ctx: JSContextRef) -> usize {
    unsafe { JSContextGetGlobalObject(ctx) as usize }
}

unsafe fn registry_ptr_from_function(ctx: JSContextRef, function: JSObjectRef) -> Option<*const ExtensionRegistry> {
    let key_name_cstr = CString::new("__otter_registry_ptr").ok()?;
    let key_name_ref = JSStringCreateWithUTF8CString(key_name_cstr.as_ptr());

    let mut exception: JSValueRef = std::ptr::null_mut();
    let value = JSObjectGetProperty(ctx, function, key_name_ref, &mut exception);
    JSStringRelease(key_name_ref);

    if !exception.is_null() || value.is_null() || JSValueIsUndefined(ctx, value) {
        return None;
    }

    let mut num_exc: JSValueRef = std::ptr::null_mut();
    let num = JSValueToNumber(ctx, value, &mut num_exc);
    if !num_exc.is_null() || !num.is_finite() || num < 0.0 {
        return None;
    }

    Some(num as usize as *const ExtensionRegistry)
}

/// Set the Tokio runtime handle for the current worker thread.
/// This should be called at worker startup to enable async operations.
pub fn set_tokio_handle(handle: tokio::runtime::Handle) {
    TOKIO_HANDLE.with(|h| {
        *h.borrow_mut() = Some(handle);
    });
}

/// Get the Tokio runtime handle for the current thread.
/// Returns the thread-local handle if set, otherwise tries `Handle::try_current()`.
fn get_tokio_handle() -> Option<tokio::runtime::Handle> {
    TOKIO_HANDLE
        .with(|h| h.borrow().clone())
        .or_else(|| tokio::runtime::Handle::try_current().ok())
}

pub(crate) fn register_context_registry(ctx: JSContextRef, registry: Arc<ExtensionRegistry>) {
    let ctx_key = ctx as usize;
    let global_key = registry_key(ctx);

    REGISTRY_MAP.with(|map| {
        let mut map = map.borrow_mut();
        map.insert(ctx_key, registry.clone());
        map.insert(global_key, registry);
    });
}

pub(crate) fn unregister_context_registry(ctx: JSContextRef) {
    let ctx_key = ctx as usize;
    let global_key = registry_key(ctx);

    REGISTRY_MAP.with(|map| {
        let mut map = map.borrow_mut();
        map.remove(&ctx_key);
        map.remove(&global_key);
    });
}

fn registry_for_context(ctx: JSContextRef) -> Option<Arc<ExtensionRegistry>> {
    let ctx_key = ctx as usize;
    let global_key = registry_key(ctx);

    REGISTRY_MAP.with(|map| {
        let map = map.borrow();
        map.get(&ctx_key).cloned().or_else(|| map.get(&global_key).cloned())
    })
}

pub(crate) fn schedule_promise<F>(ctx: JSContextRef, fut: F) -> JscResult<JSValueRef>
where
    F: Future<Output = OpResult> + Send + 'static,
{
    let registry = registry_for_context(ctx)
        .ok_or_else(|| JscError::internal("Extension registry not found".to_string()))?;

    let (promise, promise_id) = registry.create_deferred_promise(ctx)?;
    let queue = registry.async_queue.clone();
    queue.inflight_ops.fetch_add(1, Ordering::Relaxed);

    match get_tokio_handle() {
        Some(handle) => {
            handle.spawn(async move {
                let result = fut.await;
                queue.queue_result(promise_id, result);
            });
        }
        None => {
            queue.queue_result(
                promise_id,
                Err(JscError::internal(
                    "Async task executed without Tokio runtime".to_string(),
                )),
            );
        }
    }

    Ok(promise)
}

fn register_js_op(ctx: JSContextRef, name: &str, registry_ptr: usize) -> JscResult<()> {
    let name_cstr = CString::new(name).map_err(|e| JscError::internal(e.to_string()))?;

    unsafe {
        let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
        let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, Some(js_op_dispatch));

        // Tag the function with a pointer to its ExtensionRegistry so we can recover the registry
        // even if JSC invokes the callback with an unexpected JSContextRef pointer.
        let ptr_num = registry_ptr as f64;
        let key_name_cstr = CString::new("__otter_registry_ptr").unwrap();
        let key_name_ref = JSStringCreateWithUTF8CString(key_name_cstr.as_ptr());
        let key_value = JSValueMakeNumber(ctx, ptr_num);
        let mut key_exc: JSValueRef = std::ptr::null_mut();
        JSObjectSetProperty(
            ctx,
            func,
            key_name_ref,
            key_value,
            K_JS_PROPERTY_ATTRIBUTE_DONT_ENUM,
            &mut key_exc,
        );
        JSStringRelease(key_name_ref);

        let mut exception: JSValueRef = std::ptr::null_mut();
        JSObjectSetProperty(
            ctx,
            JSContextGetGlobalObject(ctx),
            name_ref,
            func as JSValueRef,
            K_JS_PROPERTY_ATTRIBUTE_NONE,
            &mut exception,
        );

        JSStringRelease(name_ref);

        if !exception.is_null() {
            return Err(JscError::script_error("Error", "Failed to register op"));
        }
    }

    Ok(())
}

unsafe extern "C" fn js_op_dispatch(
    ctx: JSContextRef,
    function: JSObjectRef,
    _this_object: JSObjectRef,
    argument_count: usize,
    arguments: *const JSValueRef,
    exception: *mut JSValueRef,
) -> JSValueRef {
    let name = match function_name(ctx, function) {
        Ok(name) => name,
        Err(err) => {
            *exception = make_exception(ctx, &err.to_string());
            return JSValueMakeUndefined(ctx);
        }
    };

    let registry = match registry_for_context(ctx) {
        Some(registry) => registry,
        None => {
            if let Some(ptr) = registry_ptr_from_function(ctx, function) {
                // SAFETY:
                // - `ptr` points to the inner value of an `Arc<ExtensionRegistry>` created by JscContext.
                // - We increment the strong count before creating a new Arc from the raw pointer.
                unsafe {
                    Arc::increment_strong_count(ptr);
                    Arc::from_raw(ptr)
                }
            } else {
                *exception = make_exception(ctx, "Extension registry not found");
                return JSValueMakeUndefined(ctx);
            }
        }
    };

    let op = match registry.get_op(&name) {
        Some(op) => op,
        None => {
            *exception = make_exception(ctx, &format!("Unknown op: {}", name));
            return JSValueMakeUndefined(ctx);
        }
    };

    let mut args = Vec::with_capacity(argument_count);
    for index in 0..argument_count {
        let value = *arguments.add(index);
        match js_value_to_json(ctx, value) {
            Ok(value) => args.push(value),
            Err(err) => {
                *exception = make_exception(ctx, &err.to_string());
                return JSValueMakeUndefined(ctx);
            }
        }
    }

    let op_context = OpContext {
        state: registry.state(),
    };

    match op.handler {
        OpHandler::Sync(handler) => match handler(op_context, args) {
            Ok(value) => json_to_js_value(ctx, &value),
            Err(err) => {
                *exception = make_exception(ctx, &err.to_string());
                JSValueMakeUndefined(ctx)
            }
        },
        OpHandler::Async(handler) => {
            let (promise, promise_id) = match registry.create_deferred_promise(ctx) {
                Ok(value) => value,
                Err(err) => {
                    *exception = make_exception(ctx, &err.to_string());
                    return JSValueMakeUndefined(ctx);
                }
            };

            let queue = registry.async_queue.clone();
            queue.inflight_ops.fetch_add(1, Ordering::Relaxed);
            match get_tokio_handle() {
                Some(handle) => {
                    handle.spawn(async move {
                        let result = handler(op_context, args).await;
                        queue.queue_result(promise_id, result);
                    });
                }
                None => {
                    queue.queue_result(
                        promise_id,
                        Err(JscError::internal(
                            "Async op executed without Tokio runtime".to_string(),
                        )),
                    );
                }
            }

            promise
        }
    }
}

unsafe fn resolve_pending(
    ctx: JSContextRef,
    promise: PendingPromise,
    result: OpResult,
) -> JscResult<()> {
    let (method, value) = match result {
        Ok(value) => (promise.resolve, value),
        Err(err) => (promise.reject, serde_json::Value::String(err.to_string())),
    };

    let js_value = json_to_js_value(ctx, &value);
    call_promise_handler(ctx, method, js_value)?;

    JSValueUnprotect(ctx, promise.resolve as JSValueRef);
    JSValueUnprotect(ctx, promise.reject as JSValueRef);

    Ok(())
}

unsafe fn call_promise_handler(
    ctx: JSContextRef,
    handler: JSObjectRef,
    value: JSValueRef,
) -> JscResult<()> {
    let args = [value];
    let mut exception: JSValueRef = std::ptr::null_mut();
    let result = JSObjectCallAsFunction(
        ctx,
        handler,
        JSContextGetGlobalObject(ctx),
        args.len(),
        args.as_ptr(),
        &mut exception,
    );

    if !exception.is_null() || result.is_null() {
        return Err(JscError::internal(
            "Promise handler call failed".to_string(),
        ));
    }

    Ok(())
}

unsafe fn function_name(ctx: JSContextRef, function: JSObjectRef) -> JscResult<String> {
    let name_key = CString::new("name").map_err(|e| JscError::internal(e.to_string()))?;
    let name_ref = JSStringCreateWithUTF8CString(name_key.as_ptr());
    let mut exception: JSValueRef = std::ptr::null_mut();
    let value = JSObjectGetProperty(ctx, function, name_ref, &mut exception);
    JSStringRelease(name_ref);

    if !exception.is_null() || value.is_null() {
        return Err(JscError::internal("Failed to read op name".to_string()));
    }

    let js_str = JSValueToStringCopy(ctx, value, &mut exception);
    if !exception.is_null() || js_str.is_null() {
        return Err(JscError::internal("Failed to read op name".to_string()));
    }

    let name = js_string_to_rust(js_str);
    JSStringRelease(js_str);
    Ok(name)
}

unsafe fn js_value_to_json(ctx: JSContextRef, value: JSValueRef) -> JscResult<serde_json::Value> {
    if value.is_null() || JSValueIsUndefined(ctx, value) {
        return Ok(serde_json::Value::Null);
    }

    let mut exception: JSValueRef = std::ptr::null_mut();
    let js_str = JSValueCreateJSONString(ctx, value, 0, &mut exception);
    if !exception.is_null() || js_str.is_null() {
        return Err(JscError::script_error(
            "Error",
            "Argument is not JSON serializable",
        ));
    }

    let json_str = js_string_to_rust(js_str);
    JSStringRelease(js_str);

    serde_json::from_str(&json_str).map_err(Into::into)
}

unsafe fn make_exception(ctx: JSContextRef, message: &str) -> JSValueRef {
    let script = format!("new Error({})", serde_json::to_string(message).unwrap());
    let script_cstr = CString::new(script).unwrap();
    let script_ref = JSStringCreateWithUTF8CString(script_cstr.as_ptr());
    let source_cstr = CString::new("<error>").unwrap();
    let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());
    let mut exc: JSValueRef = std::ptr::null_mut();

    let result = JSEvaluateScript(
        ctx,
        script_ref,
        std::ptr::null_mut(),
        source_ref,
        1,
        &mut exc,
    );

    JSStringRelease(script_ref);
    JSStringRelease(source_ref);

    if result.is_null() {
        let msg_cstr =
            CString::new(message).unwrap_or_else(|_| CString::new("Unknown error").unwrap());
        let msg_ref = JSStringCreateWithUTF8CString(msg_cstr.as_ptr());
        let str_value = JSValueMakeString(ctx, msg_ref);
        JSStringRelease(msg_ref);
        str_value
    } else {
        result
    }
}
