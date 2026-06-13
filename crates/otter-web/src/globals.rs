//! Standard Web-platform function globals: `atob`, `btoa`, `queueMicrotask`,
//! `structuredClone`, `fetch`, plus the JS-implemented class globals in
//! [`WEB_BOOTSTRAP`] (Event/EventTarget/DOMException/TextEncoder/Decoder/
//! AbortController/AbortSignal/MessageEvent/…).
//!
//! These belong to the Web platform (not Node), so they live here and are
//! installed for every runtime that enables Web APIs. `atob`/`btoa` are
//! implemented natively; `structuredClone`/`fetch` are still minimal
//! placeholders pending their conformance work.

use otter_runtime::{
    OtterError, Runtime, RuntimeGlobalInstaller, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value, SourceInput, runtime_arg_to_string,
    runtime_string_value, runtime_type_error,
};

/// Pure-JS Web Platform globals (Event/EventTarget/CustomEvent/DOMException,
/// TextEncoder/TextDecoder, performance, URLSearchParams). Evaluated once at
/// install over the already-bootstrapped intrinsics.
const WEB_BOOTSTRAP: &str = include_str!("web_bootstrap.js");

/// Installer for the Web function globals. Registered by `with_web_apis`.
#[must_use]
pub fn web_globals_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install)
}

fn install(runtime: &mut Runtime) -> Result<(), OtterError> {
    runtime.install_native_global("atob", 1, atob)?;
    runtime.install_native_global("btoa", 1, btoa)?;
    runtime.install_native_global("queueMicrotask", 1, queue_microtask)?;
    runtime.install_native_global("structuredClone", 1, structured_clone)?;
    runtime.install_native_global("fetch", 1, fetch)?;
    runtime
        .eval(SourceInput::from_javascript(WEB_BOOTSTRAP.to_string()))
        .map_err(|err| OtterError::Internal {
            code: "WEB_BOOTSTRAP".to_string(),
            message: format!("web globals bootstrap failed: {err}"),
        })?;
    Ok(())
}

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// `btoa(data)` — base64-encode a binary (latin1) string. §8.3 HTML.
fn btoa(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = runtime_arg_to_string(args, 0, ctx.heap());
    let mut bytes = Vec::with_capacity(input.len());
    for ch in input.chars() {
        let cp = ch as u32;
        if cp > 0xff {
            return Err(runtime_type_error(
                "btoa",
                "string contains characters outside the Latin1 range",
            ));
        }
        bytes.push(cp as u8);
    }
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    runtime_string_value(ctx, &out)
}

/// `atob(data)` — base64-decode to a binary (latin1) string. §8.3 HTML.
fn atob(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = runtime_arg_to_string(args, 0, ctx.heap());
    let filtered: Vec<u8> = input
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    let mut out = String::new();
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in filtered {
        let Some(v) = b64_value(c) else {
            return Err(runtime_type_error("atob", "invalid base64 character"));
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8 as char);
        }
    }
    runtime_string_value(ctx, &out)
}

// ---- placeholders (present so referencing code loads) ----

fn queue_microtask(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    // TODO: enqueue the callback on the microtask queue.
    Ok(Value::undefined())
}

fn fetch(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(runtime_type_error("fetch", "fetch is not implemented"))
}

fn structured_clone(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    otter_runtime::web_structured_clone::structured_clone(ctx, value)
}
