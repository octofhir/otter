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
use otter_runtime::{
    CapabilitySet, HostedModule, HostedModuleInstall, HostedNativeCall, OtterError, Runtime,
    RuntimeGlobalInstaller, RuntimeKeepAlive, RuntimeLiveness, RuntimeNativeCall,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeNativeFn,
    RuntimeObjectBuilder as ObjectBuilder, RuntimePersistentRootId, RuntimeTask,
    RuntimeTaskSpawner, RuntimeValue as Value, SourceInput, object, runtime_this_object,
    runtime_with_host_data,
};
use smallvec::SmallVec;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

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
              Object.defineProperty(g, '__otterServeInternals', {
                value: Object.freeze({
                  ensureFetchInternals: function () {
                    void g.Request;
                    void g.Response;
                    void g.Headers;
                    var internals = g.__otterFetchInternals;
                    if (internals == null) {
                      throw new TypeError('Otter.serve requires Web Fetch globals');
                    }
                    return internals;
                  },
                  makeRequest: function (method, url, flatHeaders, body) {
                    var internals = this.ensureFetchInternals();
                    return internals.makeRequest(method, url, flatHeaders, body);
                  },
                  responseParts: function (response) {
                    var internals = this.ensureFetchInternals();
                    var parts = internals.responseParts(response);
                    if (parts === null) return null;
                    var headersText = '';
                    for (var i = 0; i + 1 < parts[2].length; i += 2) {
                      headersText += String(parts[2][i]) + '\n' + String(parts[2][i + 1]) + '\n';
                    }
                    return {
                      status: parts[0],
                      statusText: parts[1],
                      headersText: headersText,
                      body: parts[3],
                    };
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
    let options = parse_options(ctx, args, capabilities)?;
    let listener = TcpListener::bind((options.hostname.as_str(), options.port)).map_err(|err| {
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

    let roots = ServeRoots {
        fetch: ctx.persistent_root_insert(options.fetch),
        internals: ctx.persistent_root_insert(options.internals),
        make_request: ctx.persistent_root_insert(options.fns.make_request),
        response_parts: ctx.persistent_root_insert(options.fns.response_parts),
    };
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| crate::type_error("serve", "missing execution context"))?;
    let keep_alive = task_spawner.retain_keep_alive(RuntimeLiveness::Ref);
    let control = Arc::new(ServeServerControl {
        shutdown: AtomicBool::new(false),
        keep_alive,
        roots: Mutex::new(Some(roots)),
    });
    let server = ServeServer {
        control: control.clone(),
    };
    let hostname = options.hostname.clone();
    let port = actual_addr.port();
    let url = format!("http://{actual_addr}");
    spawn_accept_loop(listener, task_spawner, context, control, roots);
    build_server_object(ctx, server, hostname, port, url)
}

#[derive(Clone, Copy)]
struct ServeFns {
    make_request: Value,
    response_parts: Value,
}

struct ServeOptions {
    hostname: String,
    port: u16,
    fetch: Value,
    internals: Value,
    fns: ServeFns,
}

#[derive(Clone, Copy)]
struct ServeRoots {
    fetch: RuntimePersistentRootId,
    internals: RuntimePersistentRootId,
    make_request: RuntimePersistentRootId,
    response_parts: RuntimePersistentRootId,
}

struct ServeServerControl {
    shutdown: AtomicBool,
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
    status_text: String,
    headers: Vec<(String, String)>,
    body: ServeBody,
}

struct ServeRequestTask {
    context: otter_runtime::RuntimeExecutionContext,
    roots: ServeRoots,
    request: HttpRequest,
    reply: mpsc::SyncSender<Result<HttpResponse, String>>,
}

impl RuntimeTask for ServeRequestTask {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let ServeRequestTask {
            context,
            roots,
            request,
            reply,
        } = *self;
        let mut response = None;
        let result = runtime.run_native_event(&context, |ctx| {
            let options = ServeDispatchOptions::from_roots(ctx, roots)?;
            let js_request = make_request(ctx, &options, &request)?;
            let response_value = call_js(
                ctx,
                "serve.fetch",
                options.fetch,
                Value::undefined(),
                smallvec::smallvec![js_request],
            )?;
            let response_value = resolve_fetch_result(ctx, response_value)?;
            response = Some(response_from_value(ctx, &options, response_value)?);
            Ok(Value::undefined())
        });
        match result {
            Ok(_) => {
                let _ = reply.send(Ok(response.expect("serve response should be set")));
            }
            Err(err) => {
                let _ = reply.send(Err(err.to_string()));
            }
        }
        Ok(())
    }
}

struct ServeDispatchOptions {
    fetch: Value,
    internals: Value,
    fns: ServeFns,
}

impl ServeDispatchOptions {
    fn from_roots(ctx: &mut NativeCtx<'_>, roots: ServeRoots) -> Result<Self, NativeError> {
        let fetch = ctx
            .persistent_root_get(roots.fetch)
            .ok_or_else(|| crate::type_error("serve", "server fetch root is closed"))?;
        let internals = ctx
            .persistent_root_get(roots.internals)
            .ok_or_else(|| crate::type_error("serve", "server internals root is closed"))?;
        let make_request = ctx
            .persistent_root_get(roots.make_request)
            .ok_or_else(|| crate::type_error("serve", "server Request factory root is closed"))?;
        let response_parts = ctx
            .persistent_root_get(roots.response_parts)
            .ok_or_else(|| {
                crate::type_error("serve", "server Response extractor root is closed")
            })?;
        Ok(Self {
            fetch,
            internals,
            fns: ServeFns {
                make_request,
                response_parts,
            },
        })
    }
}

fn spawn_accept_loop(
    listener: TcpListener,
    task_spawner: RuntimeTaskSpawner,
    context: otter_runtime::RuntimeExecutionContext,
    control: Arc<ServeServerControl>,
    roots: ServeRoots,
) {
    thread::Builder::new()
        .name("otter-serve".to_string())
        .spawn(move || {
            while !control.shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _addr)) => {
                        let task_spawner = task_spawner.clone();
                        let context = context.clone();
                        thread::spawn(move || {
                            if let Err(err) =
                                handle_stream_async(&task_spawner, context, roots, &mut stream)
                            {
                                let body = ServeBody::from_bytes(
                                    format!("Internal Server Error\n{err}").into_bytes(),
                                );
                                let _ = write_response(
                                    &mut stream,
                                    500,
                                    "Internal Server Error",
                                    &[(
                                        "content-type".to_string(),
                                        "text/plain; charset=utf-8".to_string(),
                                    )],
                                    &body,
                                );
                            }
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            control.keep_alive.close();
        })
        .expect("failed to spawn otter serve accept loop");
}

fn handle_stream_async(
    task_spawner: &RuntimeTaskSpawner,
    context: otter_runtime::RuntimeExecutionContext,
    roots: ServeRoots,
    stream: &mut TcpStream,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| format!("failed to set read timeout: {err}"))?;
    let request = read_request(stream).map_err(|err| err.to_string())?;
    let (reply, rx) = mpsc::sync_channel(1);
    task_spawner
        .enqueue(
            ServeRequestTask {
                context,
                roots,
                request,
                reply,
            },
            RuntimeLiveness::Ref,
        )
        .map_err(|err| err.to_string())?;
    let response = rx
        .recv()
        .map_err(|_| "runtime closed before request completed".to_string())??;
    write_response(
        stream,
        response.status,
        &response.status_text,
        &response.headers,
        &response.body,
    )
    .map_err(|err| err.to_string())
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
    control.keep_alive.close();
    if let Some(roots) = control.roots.lock().expect("serve roots poisoned").take() {
        let _ = ctx.persistent_root_remove(roots.fetch);
        let _ = ctx.persistent_root_remove(roots.internals);
        let _ = ctx.persistent_root_remove(roots.make_request);
        let _ = ctx.persistent_root_remove(roots.response_parts);
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
    let response_parts = object::get(internals_obj, ctx.heap(), "responseParts")
        .filter(|value| value.is_callable())
        .ok_or_else(|| crate::type_error("serve", "missing Response extractor"))?;
    Ok(ServeOptions {
        hostname,
        port,
        fetch,
        internals,
        fns: ServeFns {
            make_request,
            response_parts,
        },
    })
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, NativeError> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream
            .read(&mut chunk)
            .map_err(|err| crate::type_error("serve", format!("failed to read request: {err}")))?;
        if read == 0 {
            return Err(crate::type_error(
                "serve",
                "connection closed before request headers",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 64 * 1024 {
            return Err(crate::type_error("serve", "request headers exceed 64 KiB"));
        }
        if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let header_bytes = &buffer[..header_end - 4];
    let header_text = std::str::from_utf8(header_bytes)
        .map_err(|_| crate::type_error("serve", "request headers must be UTF-8"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| crate::type_error("serve", "missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| crate::type_error("serve", "missing request method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| crate::type_error("serve", "missing request target"))?;
    let mut headers = Vec::new();
    let mut host = None;
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(crate::type_error("serve", "malformed request header"));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "host" {
            host = Some(value.clone());
        } else if name == "content-length" {
            content_length = value.parse::<usize>().map_err(|_| {
                crate::type_error("serve", "content-length must be a non-negative integer")
            })?;
        }
        headers.push((name, value));
    }
    let mut body_bytes = buffer[header_end..].to_vec();
    if body_bytes.len() < content_length {
        let missing = content_length - body_bytes.len();
        let start = body_bytes.len();
        body_bytes.resize(content_length, 0);
        stream
            .read_exact(&mut body_bytes[start..start + missing])
            .map_err(|err| crate::type_error("serve", format!("failed to read body: {err}")))?;
    } else if body_bytes.len() > content_length {
        body_bytes.truncate(content_length);
    }
    let url = if target.starts_with("http://") || target.starts_with("https://") {
        target.to_string()
    } else {
        format!(
            "http://{}{}",
            host.unwrap_or_else(|| "127.0.0.1".to_string()),
            target
        )
    };
    Ok(HttpRequest {
        method,
        url,
        headers,
        body: ServeBody::from_bytes(body_bytes),
    })
}

fn make_request(
    ctx: &mut NativeCtx<'_>,
    options: &ServeDispatchOptions,
    request: &HttpRequest,
) -> Result<Value, NativeError> {
    let root_base = ctx.push_scratch_root(options.internals);
    let make_request_root = ctx.push_scratch_root(options.fns.make_request);
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

fn response_from_value(
    ctx: &mut NativeCtx<'_>,
    options: &ServeDispatchOptions,
    value: Value,
) -> Result<HttpResponse, NativeError> {
    let root_base = ctx.push_scratch_root(options.internals);
    let response_parts_root = ctx.push_scratch_root(options.fns.response_parts);
    let response_root = ctx.push_scratch_root(value);
    let parts = call_js(
        ctx,
        "serve.responseParts",
        ctx.scratch_root(response_parts_root),
        ctx.scratch_root(root_base),
        smallvec::smallvec![ctx.scratch_root(response_root)],
    )?;
    ctx.pop_scratch_root_to(root_base);
    if parts.is_null() {
        return Err(crate::type_error(
            "serve",
            "fetch handler must return a Response",
        ));
    }
    let Some(parts_obj) = parts.as_object() else {
        return Err(crate::type_error(
            "serve",
            "invalid Response extractor result",
        ));
    };
    let status = object::get(parts_obj, ctx.heap(), "status")
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
    let status_text = object::get(parts_obj, ctx.heap(), "statusText")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_status_text(status).to_string());
    let headers_text = object::get(parts_obj, ctx.heap(), "headersText")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|value| value.to_lossy_string(ctx.heap()))
        .unwrap_or_default();
    let mut headers = Vec::new();
    let mut lines = headers_text.split('\n');
    while let Some(name) = lines.next() {
        if name.is_empty() {
            continue;
        }
        let Some(value) = lines.next() else {
            break;
        };
        if !header_is_managed(name) {
            headers.push((name.to_string(), value.to_string()));
        }
    }
    let body_value = object::get(parts_obj, ctx.heap(), "body").unwrap_or_else(Value::null);
    let body = ServeBody::from_js_value(ctx, body_value)?;
    Ok(HttpResponse {
        status,
        status_text,
        headers,
        body,
    })
}

fn resolve_fetch_result(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    ctx.resolve_native_promise_after_microtasks(value, "serve.fetch")
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    headers: &[(String, String)],
    body: &ServeBody,
) -> Result<(), NativeError> {
    write!(stream, "HTTP/1.1 {status} {status_text}\r\n")
        .and_then(|_| write!(stream, "content-length: {}\r\n", body.len()))
        .map_err(|err| crate::type_error("serve", format!("failed to write response: {err}")))?;
    let mut has_content_type = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-type") {
            has_content_type = true;
        }
        write!(stream, "{name}: {value}\r\n").map_err(|err| {
            crate::type_error("serve", format!("failed to write response: {err}"))
        })?;
    }
    if !has_content_type {
        stream
            .write_all(b"content-type: text/plain; charset=utf-8\r\n")
            .map_err(|err| {
                crate::type_error("serve", format!("failed to write response: {err}"))
            })?;
    }
    stream
        .write_all(b"connection: close\r\n\r\n")
        .and_then(|_| stream.write_all(body.as_buffered_bytes()))
        .map_err(|err| crate::type_error("serve", format!("failed to write response: {err}")))
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

fn default_status_text(status: u16) -> &'static str {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("OK")
}
