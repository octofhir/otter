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

pub type OpResult = JscResult<serde_json::Value>;
pub type OpFuture = Pin<Box<dyn Future<Output = OpResult> + Send + 'static>>;

#[derive(Clone)]
pub struct ExtensionState {
    inner: Arc<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
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
    #[allow(dead_code)]
    name: String,
    ops: Vec<OpDecl>,
    init: Option<Arc<dyn Fn(&ExtensionState) + Send + Sync>>,
}

impl Extension {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ops: Vec::new(),
            init: None,
        }
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
        if let Some(init) = extension.init.as_ref() {
            init(&self.state);
        }

        for op in extension.ops {
            self.register_op(op, ctx)?;
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

        register_js_op(ctx, op.name())?;
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
}

pub(crate) fn register_context_registry(ctx: JSContextRef, registry: Arc<ExtensionRegistry>) {
    REGISTRY_MAP.with(|map| {
        map.borrow_mut().insert(ctx as usize, registry);
    });
}

pub(crate) fn unregister_context_registry(ctx: JSContextRef) {
    REGISTRY_MAP.with(|map| {
        map.borrow_mut().remove(&(ctx as usize));
    });
}

fn registry_for_context(ctx: JSContextRef) -> Option<Arc<ExtensionRegistry>> {
    REGISTRY_MAP.with(|map| map.borrow().get(&(ctx as usize)).cloned())
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

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let result = fut.await;
                queue.queue_result(promise_id, result);
            });
        }
        Err(_) => {
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

fn register_js_op(ctx: JSContextRef, name: &str) -> JscResult<()> {
    let name_cstr = CString::new(name).map_err(|e| JscError::internal(e.to_string()))?;

    unsafe {
        let name_ref = JSStringCreateWithUTF8CString(name_cstr.as_ptr());
        let func = JSObjectMakeFunctionWithCallback(ctx, name_ref, Some(js_op_dispatch));

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
            *exception = make_exception(ctx, "Extension registry not found");
            return JSValueMakeUndefined(ctx);
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
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        let result = handler(op_context, args).await;
                        queue.queue_result(promise_id, result);
                    });
                }
                Err(_) => {
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
        return Err(JscError::script_error("Error", "Argument is not JSON serializable"));
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
