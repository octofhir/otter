//! Otter-specific server API surface.
//!
//! This module owns the public `import { serve } from "otter"` and
//! `globalThis.Otter.serve` entry points. The HTTP backend lands in a follow-up
//! slice; this file establishes the module/global shape on the active runtime
//! stack so the server implementation has one stable public surface.
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

use otter_runtime::{
    OtterError, Runtime, RuntimeGlobalInstaller, RuntimeNativeCall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeFn, RuntimeValue as Value, SourceInput, object,
};
use smallvec::SmallVec;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    time::Duration,
};

otter_macros::lodge! {
    specifier = "otter",
    name = "otter",
    capabilities = true,
    exports = {
        "serve" / 1 => serve,
    },
}

/// Installer for the global `Otter` namespace.
#[must_use]
pub fn otter_global_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install_global_otter)
}

fn install_global_otter(runtime: &mut Runtime) -> Result<(), OtterError> {
    let capabilities = runtime.capabilities().clone();
    let serve_call: Arc<RuntimeNativeFn> =
        Arc::new(move |ctx, args, _captures| serve(ctx, args, &capabilities));
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
                  makeRequest: function (method, url, flatHeaders, body) {
                    var internals = g.__otterFetchInternals;
                    if (internals == null) {
                      throw new TypeError('Otter.serve requires Web Fetch globals');
                    }
                    return internals.makeRequest(method, url, flatHeaders, body);
                  },
                  responseParts: function (response) {
                    var internals = g.__otterFetchInternals;
                    if (internals == null) {
                      throw new TypeError('Otter.serve requires Web Fetch globals');
                    }
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
    capabilities: &otter_runtime::CapabilitySet,
) -> Result<Value, NativeError> {
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
    println!("Otter.serve listening on http://{actual_addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if let Err(err) = handle_stream(ctx, &options, &mut stream) {
                    let body = format!("Internal Server Error\n{err}");
                    let _ = write_response(
                        &mut stream,
                        500,
                        "Internal Server Error",
                        &[(
                            "content-type".to_string(),
                            "text/plain; charset=utf-8".to_string(),
                        )],
                        body.as_bytes(),
                    );
                }
            }
            Err(err) => {
                return Err(crate::type_error(
                    "serve",
                    format!("failed to accept connection: {err}"),
                ));
            }
        }
    }
    Ok(Value::undefined())
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

struct HttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn parse_options(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    capabilities: &otter_runtime::CapabilitySet,
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

fn handle_stream(
    ctx: &mut NativeCtx<'_>,
    options: &ServeOptions,
    stream: &mut TcpStream,
) -> Result<(), NativeError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| crate::type_error("serve", format!("failed to set read timeout: {err}")))?;
    let request = read_request(stream)?;
    let js_request = make_request(ctx, options, &request)?;
    let response_value = call_js(
        ctx,
        "serve.fetch",
        options.fetch,
        Value::undefined(),
        smallvec::smallvec![js_request],
    )?;
    let response = response_from_value(ctx, options, response_value)?;
    write_response(
        stream,
        response.status,
        &response.status_text,
        &response.headers,
        &response.body,
    )
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
    let mut body = buffer[header_end..].to_vec();
    if body.len() < content_length {
        let missing = content_length - body.len();
        let start = body.len();
        body.resize(content_length, 0);
        stream
            .read_exact(&mut body[start..start + missing])
            .map_err(|err| crate::type_error("serve", format!("failed to read body: {err}")))?;
    } else if body.len() > content_length {
        body.truncate(content_length);
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
        body,
    })
}

fn make_request(
    ctx: &mut NativeCtx<'_>,
    options: &ServeOptions,
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
    let body_root = if request.body.is_empty() {
        None
    } else {
        let body = String::from_utf8_lossy(&request.body);
        Some(push_string_root(ctx, &body)?)
    };
    let body = body_root
        .map(|idx| ctx.scratch_root(idx))
        .unwrap_or_else(Value::null);
    let args = smallvec::smallvec![
        ctx.scratch_root(method_root),
        ctx.scratch_root(url_root),
        ctx.scratch_root(flat_headers_root),
        body,
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
    options: &ServeOptions,
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
    let body = body_bytes(ctx, body_value)?;
    Ok(HttpResponse {
        status,
        status_text,
        headers,
        body,
    })
}

fn body_bytes(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Vec<u8>, NativeError> {
    if value.is_null() || value.is_undefined() {
        return Ok(Vec::new());
    }
    if let Some(string) = value.as_string(ctx.heap()) {
        return Ok(string.to_lossy_string(ctx.heap()).into_bytes());
    }
    if let Some(typed_array) = value.as_typed_array(ctx.heap()) {
        let offset = typed_array.byte_offset(ctx.heap());
        let len = typed_array.byte_length(ctx.heap());
        return Ok(typed_array
            .buffer(ctx.heap())
            .with_bytes(ctx.heap(), |bytes| {
                bytes.get(offset..offset + len).map(<[u8]>::to_vec)
            })
            .unwrap_or_default());
    }
    if let Some(buffer) = value.as_array_buffer() {
        return Ok(buffer.with_bytes(ctx.heap(), |bytes| bytes.to_vec()));
    }
    Err(crate::type_error(
        "serve",
        "Response body streams are not supported yet; return a buffered Response body",
    ))
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    headers: &[(String, String)],
    body: &[u8],
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
        .and_then(|_| stream.write_all(body))
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
