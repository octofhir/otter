//! Otter-specific server API surface.
//!
//! This module owns the public `import { serve } from "otter"` and
//! `globalThis.Otter.serve` entry points.
//!
//! # Contents
//! - Hosted module registration for the bare `"otter"` specifier.
//! - Global `Otter` namespace installer.
//! - The `serve` native entry point.
//!
//! # Invariants
//! - Otter-specific APIs live in `otter-modules`, not in `otter-web` or
//!   `otter-runtime`.
//! - The Web Fetch classes remain owned by `otter-web`; server request/response
//!   conversion will use their hidden plain-data factory.
//! - No VM handles or contexts are stored in long-lived host state.
//!
//! # See also
//! - [`crate::hosted_modules`]

mod body;

use body::ServeBody;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request as HyperRequest, Response as HyperResponse};
use hyper_util::rt::TokioIo;
use otter_runtime::{
    CapabilitySet, HostedModule, HostedModuleInstall, HostedNativeCall, OtterError, Runtime,
    RuntimeGlobalInstaller, RuntimeKeepAlive, RuntimeLiveness, RuntimeNativeCall,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeNativeFn,
    RuntimeObjectBuilder as ObjectBuilder, RuntimePersistentRootId, RuntimeTask,
    RuntimeTaskSpawner, RuntimeValue as Value, SourceInput, object, runtime_this_object,
    runtime_with_host_data,
};
use smallvec::SmallVec;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{Notify, oneshot};

/// Per-server table of in-flight replies keyed by a monotonic token. A request
/// registers its oneshot sender, then the `deliver`/`deliverError` natives —
/// fired as promise reactions during a normal microtask drain — take the sender
/// back by token and settle it. This decouples handler completion from the
/// dispatch call so async handlers deliver on a later drain instead of forcing
/// an inline microtask flush per request (the old reg-window leak).
#[derive(Default)]
struct ReplyRegistry {
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<HttpResponse, String>>>>,
    next: AtomicU64,
}

impl ReplyRegistry {
    fn register(&self, reply: oneshot::Sender<Result<HttpResponse, String>>) -> u64 {
        let token = self.next.fetch_add(1, Ordering::Relaxed);
        self.pending
            .lock()
            .expect("serve reply registry poisoned")
            .insert(token, reply);
        token
    }

    fn take(&self, token: u64) -> Option<oneshot::Sender<Result<HttpResponse, String>>> {
        self.pending
            .lock()
            .expect("serve reply registry poisoned")
            .remove(&token)
    }
}

/// Static hosted module row for `import { serve } from "otter"`.
pub static OTTER_HOSTED_MODULE: HostedModule =
    HostedModule::new("otter", HostedModuleInstall::new(install_otter_module));

/// Install the bare `"otter"` hosted module.
pub fn install_otter_module(ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    let capabilities = ctx.capabilities().clone();
    let task_spawner = ctx.runtime_task_spawner();
    let serve_call: Arc<RuntimeNativeFn> =
        Arc::new(move |ctx, args, _captures| serve(ctx, args, &capabilities, task_spawner.clone()));
    ctx.method("serve", 1, HostedNativeCall::dynamic(serve_call))?;
    Ok(())
}

/// Installer for the global `Otter` namespace.
#[must_use]
pub fn otter_global_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install_global_otter)
}

fn install_global_otter(runtime: &mut Runtime) -> Result<(), OtterError> {
    let capabilities = runtime.capabilities().clone();
    let task_spawner = runtime.runtime_task_spawner();
    let serve_call: Arc<RuntimeNativeFn> =
        Arc::new(move |ctx, args, _captures| serve(ctx, args, &capabilities, task_spawner.clone()));
    runtime.install_native_global_call(
        "__otterServe",
        1,
        RuntimeNativeCall::Dynamic(serve_call),
    )?;
    runtime
        .eval(SourceInput::from_javascript(
            r#"
            (function (g) {
              'use strict';
              var serve = g.__otterServe;
              delete g.__otterServe;
              var ns = g.Otter;
              if (ns == null || (typeof ns !== 'object' && typeof ns !== 'function')) {
                ns = {};
              }
              Object.defineProperty(ns, 'serve', {
                value: serve,
                writable: true,
                enumerable: true,
                configurable: true,
              });
              Object.defineProperty(g, 'Otter', {
                value: ns,
                writable: true,
                enumerable: false,
                configurable: true,
              });
              // Resolve the Fetch internals once and cache them in this
              // closure; the fast path per request must not re-walk globals.
              var fetchInternals = null;
              function ensureFetch() {
                if (fetchInternals !== null) return fetchInternals;
                void g.Request;
                void g.Response;
                void g.Headers;
                var internals = g.__otterFetchInternals;
                if (internals == null) {
                  throw new TypeError('Otter.serve requires Web Fetch globals');
                }
                fetchInternals = internals;
                return internals;
              }
              Object.defineProperty(g, '__otterServeInternals', {
                value: Object.freeze({
                  makeRequest: function (method, url, flatHeaders, body) {
                    var internals = fetchInternals || ensureFetch();
                    return internals.makeRequest(method, url, flatHeaders, body);
                  },
                  // Hand the private Fetch slot symbols to the native server once,
                  // so it reads a Response's status/headers/body directly in Rust.
                  fetchSlots: function () {
                    return ensureFetch().slots;
                  },
                  dispatch: function (handler, request, deliver, deliverError, token) {
                    try {
                      var result = handler(request);
                      // `deliver` extracts the Response natively. A sync handler
                      // delivers inline (no promise/microtask); only a genuine
                      // thenable parks until a later drain.
                      if (
                        result !== null &&
                        typeof result === 'object' &&
                        typeof result.then === 'function'
                      ) {
                        result.then(
                          function (response) { deliver(token, response); },
                          function (err) { deliverError(token, err); }
                        );
                      } else {
                        deliver(token, result);
                      }
                    } catch (err) {
                      deliverError(token, err);
                    }
                  },
                }),
                writable: false,
                enumerable: false,
                configurable: true,
              });
            })(globalThis);
            "#,
        ))
        .map_err(|err| OtterError::Internal {
            code: "OTTER_GLOBAL_INSTALL".to_string(),
            message: format!("Otter global install failed: {err}"),
        })?;
    Ok(())
}

fn serve(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
    task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Value, NativeError> {
    let task_spawner = task_spawner
        .ok_or_else(|| crate::type_error("serve", "Otter.serve requires a runtime event loop"))?;
    let io_handle = task_spawner
        .io_handle()
        .ok_or_else(|| crate::type_error("serve", "Otter.serve requires a runtime event loop"))?;
    let options = parse_options(ctx, args, capabilities)?;
    // Bind synchronously so the returned `server.url`/`server.port` are exact
    // (including an OS-assigned port when `port: 0`). The std listener is handed
    // to the async accept loop, which drives it on the shared Tokio runtime.
    let listener =
        std::net::TcpListener::bind((options.hostname.as_str(), options.port)).map_err(|err| {
            crate::type_error(
                "serve",
                format!(
                    "failed to listen on {}:{}: {err}",
                    options.hostname, options.port
                ),
            )
        })?;
    let actual_addr = listener.local_addr().map_err(|err| {
        crate::type_error("serve", format!("failed to read local address: {err}"))
    })?;
    listener.set_nonblocking(true).map_err(|err| {
        crate::type_error("serve", format!("failed to configure listener: {err}"))
    })?;

    let registry = Arc::new(ReplyRegistry::default());
    let internals_root = ctx.persistent_root_insert(options.internals);
    // Force the Fetch globals to initialize and grab the private slot symbols so
    // `deliver` can read Response fields natively.
    let slots_value = call_js(
        ctx,
        "serve.fetchSlots",
        options.fns.fetch_slots,
        options.internals,
        smallvec::smallvec![],
    )?;
    let slots = ServeSlots::resolve(ctx, slots_value)?;
    let deliver = {
        let registry = registry.clone();
        ctx.native_value_with_captures(
            "serve.deliver",
            smallvec::smallvec![],
            &[],
            &[],
            move |ctx, args, _captures| deliver_reply(ctx, &registry, slots, args),
        )
        .map_err(|err| crate::type_error("serve", err.to_string()))?
    };
    let deliver_error = {
        let registry = registry.clone();
        ctx.native_value_with_captures(
            "serve.deliverError",
            smallvec::smallvec![],
            &[],
            &[],
            move |ctx, args, _captures| deliver_error(ctx, &registry, args),
        )
        .map_err(|err| crate::type_error("serve", err.to_string()))?
    };
    let roots = ServeRoots {
        fetch: ctx.persistent_root_insert(options.fetch),
        internals: internals_root,
        make_request: ctx.persistent_root_insert(options.fns.make_request),
        dispatch: ctx.persistent_root_insert(options.fns.dispatch),
        deliver: ctx.persistent_root_insert(deliver),
        deliver_error: ctx.persistent_root_insert(deliver_error),
        slots,
    };
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| crate::type_error("serve", "missing execution context"))?;
    let keep_alive = task_spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let control = Arc::new(ServeServerControl {
        shutdown: AtomicBool::new(false),
        shutdown_signal: Notify::new(),
        keep_alive,
        roots: Mutex::new(Some(roots)),
    });
    let server = ServeServer {
        control: control.clone(),
    };
    let hostname = options.hostname.clone();
    let port = actual_addr.port();
    let url = format!("http://{actual_addr}");
    io_handle.spawn(accept_loop(
        listener,
        task_spawner,
        context,
        control,
        roots,
        registry,
    ));
    build_server_object(ctx, server, hostname, port, url)
}

#[derive(Clone, Copy)]
struct ServeFns {
    make_request: Value,
    fetch_slots: Value,
    dispatch: Value,
}

struct ServeOptions {
    hostname: String,
    port: u16,
    fetch: Value,
    internals: Value,
    fns: ServeFns,
}

/// Persistent roots for the private Fetch slot symbols, resolved once at
/// `serve()` time. The `deliver` native reads a Response's own symbol-keyed
/// slots through these to build an [`HttpResponse`] in Rust — no `responseParts`
/// JS round-trip, intermediate arrays, or header string per request.
#[derive(Clone, Copy)]
struct ServeSlots {
    status: RuntimePersistentRootId,
    status_text: RuntimePersistentRootId,
    headers: RuntimePersistentRootId,
    header_list: RuntimePersistentRootId,
    body_text: RuntimePersistentRootId,
    body_bytes: RuntimePersistentRootId,
}

impl ServeSlots {
    fn resolve(ctx: &mut NativeCtx<'_>, slots: Value) -> Result<Self, NativeError> {
        let obj = slots
            .as_object()
            .ok_or_else(|| crate::type_error("serve", "invalid Fetch slot table"))?;
        let mut symbol_root = |name: &str| -> Result<RuntimePersistentRootId, NativeError> {
            let value = object::get(obj, ctx.heap(), name).ok_or_else(|| {
                crate::type_error("serve", format!("missing Fetch slot `{name}`"))
            })?;
            Ok(ctx.persistent_root_insert(value))
        };
        Ok(Self {
            status: symbol_root("status")?,
            status_text: symbol_root("statusText")?,
            headers: symbol_root("headers")?,
            header_list: symbol_root("headerList")?,
            body_text: symbol_root("bodyText")?,
            body_bytes: symbol_root("bodyBytes")?,
        })
    }

    fn remove(self, ctx: &mut NativeCtx<'_>) {
        for id in [
            self.status,
            self.status_text,
            self.headers,
            self.header_list,
            self.body_text,
            self.body_bytes,
        ] {
            let _ = ctx.persistent_root_remove(id);
        }
    }
}

#[derive(Clone, Copy)]
struct ServeRoots {
    fetch: RuntimePersistentRootId,
    internals: RuntimePersistentRootId,
    make_request: RuntimePersistentRootId,
    dispatch: RuntimePersistentRootId,
    deliver: RuntimePersistentRootId,
    deliver_error: RuntimePersistentRootId,
    slots: ServeSlots,
}

struct ServeServerControl {
    shutdown: AtomicBool,
    /// Wakes the accept loop so it stops taking new connections promptly on
    /// `server.stop()` instead of only noticing the flag on the next accept.
    shutdown_signal: Notify,
    keep_alive: RuntimeKeepAlive,
    roots: Mutex<Option<ServeRoots>>,
}

#[derive(Clone)]
struct ServeServer {
    control: Arc<ServeServerControl>,
}

struct HttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: ServeBody,
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: ServeBody,
}

struct ServeRequestTask {
    context: otter_runtime::RuntimeExecutionContext,
    roots: ServeRoots,
    request: HttpRequest,
    reply: oneshot::Sender<Result<HttpResponse, String>>,
    registry: Arc<ReplyRegistry>,
}

impl RuntimeTask for ServeRequestTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let ServeRequestTask {
            context,
            roots,
            request,
            reply,
            registry,
        } = *self;
        // Hand the reply to the registry and drive the JS dispatch trampoline.
        // For a sync handler the reaction fires inside this event's single
        // microtask drain and `deliver` settles the token before we return; an
        // async handler's reaction settles on a later drain, so the token stays
        // parked in the registry and we must NOT touch `reply` here.
        let token = registry.register(reply);
        let result = runtime.run_native_event(&context, |ctx| {
            let options = ServeDispatchOptions::from_roots(ctx, roots)?;
            let js_request = make_request(ctx, &options, &request)?;
            call_js(
                ctx,
                "serve.dispatch",
                options.dispatch,
                options.internals,
                smallvec::smallvec![
                    options.fetch,
                    js_request,
                    options.deliver,
                    options.deliver_error,
                    Value::number_f64(token as f64),
                ],
            )?;
            Ok(Value::undefined())
        });
        // The trampoline catches handler errors itself; a hard `Err` here means
        // dispatch failed before scheduling any reaction. Unblock the client if
        // the token is still parked.
        if let Err(err) = result {
            if let Some(reply) = registry.take(token) {
                let _ = reply.send(Err(err.to_string()));
            }
        }
        Ok(())
    }
}

fn deliver_reply(
    ctx: &mut NativeCtx<'_>,
    registry: &ReplyRegistry,
    slots: ServeSlots,
    args: &[Value],
) -> Result<Value, NativeError> {
    let Some(token) = token_arg(args) else {
        return Ok(Value::undefined());
    };
    // `args[1]` is the raw `Response` the handler returned. Read its status,
    // headers, and body straight out of the private symbol slots in Rust — no
    // `responseParts` JS call, intermediate arrays, or header string.
    let response = args.get(1).copied().unwrap_or_else(Value::null);
    let result = extract_response(ctx, slots, response);
    if let Some(reply) = registry.take(token) {
        let _ = reply.send(result.map_err(|err| err.to_string()));
    }
    Ok(Value::undefined())
}

fn deliver_error(
    ctx: &mut NativeCtx<'_>,
    registry: &ReplyRegistry,
    args: &[Value],
) -> Result<Value, NativeError> {
    let Some(token) = token_arg(args) else {
        return Ok(Value::undefined());
    };
    let message = args
        .get(1)
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
        .unwrap_or_else(|| "fetch handler rejected".to_string());
    if let Some(reply) = registry.take(token) {
        let _ = reply.send(Err(message));
    }
    Ok(Value::undefined())
}

fn token_arg(args: &[Value]) -> Option<u64> {
    let token = args.first().and_then(|value| value.as_number()).map(|n| n.as_f64())?;
    token.is_finite().then_some(token as u64)
}

struct ServeDispatchOptions {
    fetch: Value,
    internals: Value,
    make_request: Value,
    dispatch: Value,
    deliver: Value,
    deliver_error: Value,
}

impl ServeDispatchOptions {
    fn from_roots(ctx: &mut NativeCtx<'_>, roots: ServeRoots) -> Result<Self, NativeError> {
        let get = |ctx: &NativeCtx<'_>, id, what| {
            ctx.persistent_root_get(id)
                .ok_or_else(|| crate::type_error("serve", format!("server {what} root is closed")))
        };
        Ok(Self {
            fetch: get(ctx, roots.fetch, "fetch")?,
            internals: get(ctx, roots.internals, "internals")?,
            make_request: get(ctx, roots.make_request, "Request factory")?,
            dispatch: get(ctx, roots.dispatch, "dispatch")?,
            deliver: get(ctx, roots.deliver, "deliver")?,
            deliver_error: get(ctx, roots.deliver_error, "deliverError")?,
        })
    }
}

/// Accept connections on the shared Tokio runtime, serving each with HTTP/1.1
/// keep-alive through hyper. Each connection runs concurrently; per-request VM
/// re-entry stays on the isolate thread via [`RuntimeTaskSpawner::enqueue`].
async fn accept_loop(
    listener: std::net::TcpListener,
    task_spawner: RuntimeTaskSpawner,
    context: otter_runtime::RuntimeExecutionContext,
    control: Arc<ServeServerControl>,
    roots: ServeRoots,
    registry: Arc<ReplyRegistry>,
) {
    let listener = match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(_) => {
            control.keep_alive.close();
            return;
        }
    };
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            break;
        }
        let accepted = tokio::select! {
            result = listener.accept() => result,
            () = control.shutdown_signal.notified() => break,
        };
        let Ok((stream, _addr)) = accepted else {
            break;
        };
        let task_spawner = task_spawner.clone();
        let context = context.clone();
        let registry = registry.clone();
        // One task per connection; hyper reads successive keep-alive requests
        // off it until the peer closes or the connection goes idle.
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req: HyperRequest<Incoming>| {
                let task_spawner = task_spawner.clone();
                let context = context.clone();
                let registry = registry.clone();
                async move { serve_one(&task_spawner, context, roots, registry, req).await }
            });
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(io, service)
                .await;
        });
    }
    control.keep_alive.close();
}

/// Convert one hyper request into the isolate's `fetch` dispatch and back.
async fn serve_one(
    task_spawner: &RuntimeTaskSpawner,
    context: otter_runtime::RuntimeExecutionContext,
    roots: ServeRoots,
    registry: Arc<ReplyRegistry>,
    req: HyperRequest<Incoming>,
) -> Result<HyperResponse<Full<Bytes>>, std::convert::Infallible> {
    match dispatch_request(task_spawner, context, roots, registry, req).await {
        Ok(response) => Ok(build_hyper_response(response)),
        Err(err) => Ok(error_response(&err)),
    }
}

async fn dispatch_request(
    task_spawner: &RuntimeTaskSpawner,
    context: otter_runtime::RuntimeExecutionContext,
    roots: ServeRoots,
    registry: Arc<ReplyRegistry>,
    req: HyperRequest<Incoming>,
) -> Result<HttpResponse, String> {
    let request = read_hyper_request(req).await?;
    let (reply, rx) = oneshot::channel();
    task_spawner
        .enqueue(
            ServeRequestTask {
                context,
                roots,
                request,
                reply,
                registry,
            },
            RuntimeLiveness::Ref,
        )
        .map_err(|err| err.to_string())?;
    rx.await
        .map_err(|_| "runtime closed before request completed".to_string())?
}

/// Build the engine's [`HttpRequest`] from a hyper request, resolving the
/// absolute URL from the request target and `Host` header and buffering the
/// body (hyper handles Content-Length and chunked transfer decoding).
async fn read_hyper_request(req: HyperRequest<Incoming>) -> Result<HttpRequest, String> {
    let method = req.method().as_str().to_string();
    let mut host: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::with_capacity(req.headers().len());
    for (name, value) in req.headers() {
        let value = value
            .to_str()
            .map_err(|_| "request header value is not valid UTF-8".to_string())?
            .to_string();
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "host" {
            host = Some(value.clone());
        }
        headers.push((lower, value));
    }
    let uri = req.uri();
    let url = if uri.authority().is_some() {
        // Absolute-form target (proxy-style) already carries scheme + authority.
        uri.to_string()
    } else {
        let authority = host
            .as_deref()
            .or_else(|| uri.host())
            .unwrap_or("localhost");
        let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
        format!("http://{authority}{path}")
    };
    let collected = req
        .into_body()
        .collect()
        .await
        .map_err(|err| format!("failed to read request body: {err}"))?;
    let body = ServeBody::from_bytes(collected.to_bytes().to_vec());
    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn build_hyper_response(response: HttpResponse) -> HyperResponse<Full<Bytes>> {
    let mut builder = HyperResponse::builder().status(response.status);
    let mut has_content_type = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        builder = builder.header(name.as_str(), value.as_str());
    }
    if !has_content_type {
        builder = builder.header("content-type", "text/plain;charset=UTF-8");
    }
    let bytes = Bytes::from(response.body.as_buffered_bytes().to_vec());
    builder
        .body(Full::new(bytes))
        .unwrap_or_else(|_| error_response("failed to build response"))
}

fn error_response(message: &str) -> HyperResponse<Full<Bytes>> {
    HyperResponse::builder()
        .status(500)
        .header("content-type", "text/plain;charset=UTF-8")
        .body(Full::new(Bytes::from(format!(
            "Internal Server Error\n{message}"
        ))))
        .expect("static 500 response is always valid")
}

fn build_server_object(
    ctx: &mut NativeCtx<'_>,
    server: ServeServer,
    hostname: String,
    port: u16,
    url: String,
) -> Result<Value, NativeError> {
    let hostname = crate::string_value(ctx, &hostname)?;
    let url = crate::string_value(ctx, &url)?;
    let mut builder = ObjectBuilder::from_host_data(ctx, server)?;
    builder
        .readonly_property("hostname", hostname)
        .and_then(|builder| builder.readonly_property("port", Value::number_f64(port as f64)))
        .and_then(|builder| builder.readonly_property("url", url))
        .and_then(|builder| builder.builtin_method("stop", 0, server_stop))
        .and_then(|builder| builder.builtin_method("close", 0, server_stop))
        .and_then(|builder| builder.builtin_method("ref", 0, server_ref))
        .and_then(|builder| builder.builtin_method("unref", 0, server_unref))
        .map_err(|err| crate::type_error("serve", err.to_string()))?;
    Ok(Value::object(builder.build()))
}

fn server_receiver(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<otter_runtime::RuntimeJsObject, NativeError> {
    runtime_this_object(ctx, name, "ServeServer")
}

fn server_control(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<Arc<ServeServerControl>, NativeError> {
    let object = server_receiver(ctx, name)?;
    runtime_with_host_data::<ServeServer, _>(ctx, object, |server| server.control.clone())
        .map_err(|err| crate::type_error(name, err.to_string()))
}

fn server_stop(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let control = server_control(ctx, "ServeServer.stop")?;
    control.shutdown.store(true, Ordering::Release);
    control.shutdown_signal.notify_waiters();
    control.keep_alive.close();
    if let Some(roots) = control.roots.lock().expect("serve roots poisoned").take() {
        let _ = ctx.persistent_root_remove(roots.fetch);
        let _ = ctx.persistent_root_remove(roots.internals);
        let _ = ctx.persistent_root_remove(roots.make_request);
        let _ = ctx.persistent_root_remove(roots.dispatch);
        let _ = ctx.persistent_root_remove(roots.deliver);
        let _ = ctx.persistent_root_remove(roots.deliver_error);
        roots.slots.remove(ctx);
    }
    Ok(Value::undefined())
}

fn server_ref(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let control = server_control(ctx, "ServeServer.ref")?;
    control.keep_alive.ref_();
    Ok(*ctx.this_value())
}

fn server_unref(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let control = server_control(ctx, "ServeServer.unref")?;
    control.keep_alive.unref();
    Ok(*ctx.this_value())
}

fn parse_options(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &CapabilitySet,
) -> Result<ServeOptions, NativeError> {
    let options = args
        .first()
        .and_then(|value| value.as_object())
        .ok_or_else(|| crate::type_error("serve", "expected an options object"))?;
    let fetch = object::get(options, ctx.heap(), "fetch")
        .filter(|value| value.is_callable())
        .ok_or_else(|| crate::type_error("serve", "options.fetch must be callable"))?;
    let hostname = object::get(options, ctx.heap(), "hostname")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = object::get(options, ctx.heap(), "port")
        .and_then(|value| value.as_number())
        .map(|value| value.as_f64())
        .unwrap_or(3000.0);
    if !port.is_finite() || port.fract() != 0.0 || !(0.0..=65535.0).contains(&port) {
        return Err(crate::type_error(
            "serve",
            "options.port must be an integer 0..65535",
        ));
    }
    let port = port as u16;
    let permission_target = format!("{hostname}:{port}");
    if !capabilities.net.matches(&permission_target) {
        return Err(crate::type_error(
            "serve",
            format!("network permission denied for `{permission_target}`"),
        ));
    }
    let internals = ctx
        .global_value("__otterServeInternals")
        .ok_or_else(|| crate::type_error("serve", "missing Otter serve internals"))?;
    let internals_obj = internals
        .as_object()
        .ok_or_else(|| crate::type_error("serve", "invalid Otter serve internals"))?;
    let make_request = object::get(internals_obj, ctx.heap(), "makeRequest")
        .filter(|value| value.is_callable())
        .ok_or_else(|| crate::type_error("serve", "missing Request factory"))?;
    let fetch_slots = object::get(internals_obj, ctx.heap(), "fetchSlots")
        .filter(|value| value.is_callable())
        .ok_or_else(|| crate::type_error("serve", "missing Fetch slot accessor"))?;
    let dispatch = object::get(internals_obj, ctx.heap(), "dispatch")
        .filter(|value| value.is_callable())
        .ok_or_else(|| crate::type_error("serve", "missing dispatch trampoline"))?;
    Ok(ServeOptions {
        hostname,
        port,
        fetch,
        internals,
        fns: ServeFns {
            make_request,
            fetch_slots,
            dispatch,
        },
    })
}

fn make_request(
    ctx: &mut NativeCtx<'_>,
    options: &ServeDispatchOptions,
    request: &HttpRequest,
) -> Result<Value, NativeError> {
    let root_base = ctx.push_scratch_root(options.internals);
    let make_request_root = ctx.push_scratch_root(options.make_request);
    let method_root = push_string_root(ctx, &request.method)?;
    let url_root = push_string_root(ctx, &request.url)?;
    let mut header_roots = Vec::with_capacity(request.headers.len() * 2);
    for (name, value) in &request.headers {
        header_roots.push(push_string_root(ctx, name)?);
        header_roots.push(push_string_root(ctx, value)?);
    }
    let flat_headers = header_roots
        .iter()
        .map(|idx| ctx.scratch_root(*idx))
        .collect::<Vec<_>>();
    let flat_headers = Value::array(
        ctx.array_from_elements(flat_headers)
            .map_err(|err| crate::type_error("serve", err.to_string()))?,
    );
    let flat_headers_root = ctx.push_scratch_root(flat_headers);
    let body = request.body.to_js_body(ctx)?;
    let body_root = ctx.push_scratch_root(body);
    let args = smallvec::smallvec![
        ctx.scratch_root(method_root),
        ctx.scratch_root(url_root),
        ctx.scratch_root(flat_headers_root),
        ctx.scratch_root(body_root),
    ];
    let value = call_js(
        ctx,
        "serve.makeRequest",
        ctx.scratch_root(make_request_root),
        ctx.scratch_root(root_base),
        args,
    );
    ctx.pop_scratch_root_to(root_base);
    value
}

/// Build an [`HttpResponse`] by reading a `Response`'s private Fetch slots
/// directly. Runs inside the `deliver` native; the slot symbols were resolved
/// once at `serve()` time. The HTTP/1.1 reason phrase is not observable to a
/// Fetch client and hyper derives the canonical phrase from the status code, so
/// `statusText` is intentionally not carried onto the wire.
fn extract_response(
    ctx: &mut NativeCtx<'_>,
    slots: ServeSlots,
    response: Value,
) -> Result<HttpResponse, NativeError> {
    let Some(obj) = response.as_object() else {
        return Err(crate::type_error(
            "serve",
            "fetch handler must return a Response",
        ));
    };
    let symbol = |ctx: &NativeCtx<'_>, id| {
        ctx.persistent_root_get(id)
            .and_then(|value| value.as_symbol(ctx.heap()))
            .ok_or_else(|| crate::type_error("serve", "Fetch slot symbol is closed"))
    };
    let status_sym = symbol(ctx, slots.status)?;
    let headers_sym = symbol(ctx, slots.headers)?;
    let header_list_sym = symbol(ctx, slots.header_list)?;
    let body_text_sym = symbol(ctx, slots.body_text)?;
    let body_bytes_sym = symbol(ctx, slots.body_bytes)?;

    let status = object::get_own_symbol(obj, ctx.heap(), status_sym)
        .and_then(|value| value.as_number())
        .map(|value| value.as_f64())
        .unwrap_or(200.0);
    if !status.is_finite() || status.fract() != 0.0 || !(100.0..=999.0).contains(&status) {
        return Err(crate::type_error(
            "serve",
            "Response status must be an integer",
        ));
    }
    let status = status as u16;

    // `kHeaders` -> Headers object -> `kHeaderList` array of `[name, value]`
    // pairs (lowercased names, insertion order — hyper writes them as-is).
    let mut headers = Vec::new();
    if let Some(headers_obj) =
        object::get_own_symbol(obj, ctx.heap(), headers_sym).and_then(|value| value.as_object())
        && let Some(list) = object::get_own_symbol(headers_obj, ctx.heap(), header_list_sym)
            .and_then(|value| value.as_array())
    {
        for i in 0..otter_runtime::array::len(list, ctx.heap()) {
            let Some(pair) = otter_runtime::array::get(list, ctx.heap(), i).as_array() else {
                continue;
            };
            let name = otter_runtime::array::get(pair, ctx.heap(), 0)
                .as_string(ctx.heap())
                .map(|value| value.to_lossy_string(ctx.heap()));
            let value = otter_runtime::array::get(pair, ctx.heap(), 1)
                .as_string(ctx.heap())
                .map(|value| value.to_lossy_string(ctx.heap()));
            if let (Some(name), Some(value)) = (name, value)
                && !header_is_managed(&name)
            {
                headers.push((name, value));
            }
        }
    }

    // At most one body slot is non-null. Read it last: `ServeBody::from_js_value`
    // takes `&mut`, and nothing above allocates on the GC heap.
    let body_value = {
        let text = object::get_own_symbol(obj, ctx.heap(), body_text_sym).unwrap_or_else(Value::null);
        if !text.is_null() && !text.is_undefined() {
            text
        } else {
            object::get_own_symbol(obj, ctx.heap(), body_bytes_sym).unwrap_or_else(Value::null)
        }
    };
    let body = ServeBody::from_js_value(ctx, body_value)?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn call_js(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    callee: Value,
    this_value: Value,
    args: SmallVec<[Value; 8]>,
) -> Result<Value, NativeError> {
    let (interp, context) = ctx.interp_mut_and_context();
    let context = context.ok_or_else(|| crate::type_error(name, "missing execution context"))?;
    match interp.run_callable_sync(&context, &callee, this_value, args) {
        Ok(value) => Ok(value),
        Err(err) => {
            if let Some(thrown) = interp.take_pending_uncaught_throw() {
                Err(NativeError::Thrown {
                    name,
                    message: thrown.display_string(interp.gc_heap()),
                })
            } else {
                Err(crate::type_error(name, err.to_string()))
            }
        }
    }
}

fn push_string_root(ctx: &mut NativeCtx<'_>, value: &str) -> Result<usize, NativeError> {
    let value = crate::string_value(ctx, value)?;
    Ok(ctx.push_scratch_root(value))
}

fn header_is_managed(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("connection")
}
