//! Standard Web-platform function globals: `atob`, `btoa`, `queueMicrotask`,
//! `structuredClone`, `fetch`, plus the JS-implemented class globals in
//! [`WEB_BOOTSTRAP`] (Event/EventTarget/DOMException/TextEncoder/Decoder/
//! AbortController/AbortSignal/MessageEvent/…).
//!
//! These belong to the Web platform (not Node), so they live here and are
//! installed for every runtime that enables Web APIs. `atob`/`btoa`,
//! `queueMicrotask`, `navigator`, the native in-realm `structuredClone`
//! plus Streams compression codec, and the native CSPRNG/digest backing
//! for `crypto` (see [`crate::crypto`]) are implemented. `fetch()` is a JS
//! shim over the private `__nativeFetch` transport (see [`crate::fetch_ext`]).

use std::sync::Arc;

use otter_runtime::{
    OtterError, RuntimeExtensionContext, RuntimeExtensionInstaller, RuntimeNativeCall,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeNativeFn,
    RuntimeValue as Value, SourceInput, runtime_arg_to_string, runtime_string_value,
    runtime_type_error,
};

/// Pure-JS Web Platform globals — the sources live in the `romp!`
/// declaration ([`crate::WEB_EXTENSION`]); these test-only copies feed
/// the def-scan honesty check below.
#[cfg(test)]
const WEB_BOOTSTRAP: &str = include_str!("web_bootstrap.js");

#[cfg(test)]
const WEB_STREAMS: &str = include_str!("web_streams.js");

#[cfg(test)]
const WEB_FETCH: &str = include_str!("web_fetch.js");

#[cfg(test)]
const WEB_URLPATTERN: &str = include_str!("web_urlpattern.js");

/// Installer for the Web function globals. Registered by `with_web_apis`.
#[must_use]
pub fn web_globals_installer() -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(install)
}

fn install(runtime: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    runtime.install_native_global("atob", 1, atob)?;
    runtime.install_native_global("btoa", 1, btoa)?;
    runtime.install_native_global("queueMicrotask", 1, queue_microtask)?;
    runtime.install_native_global("structuredClone", 1, structured_clone)?;
    // `fetch()` itself is the JS shim in `web_fetch.js`; it normalizes its
    // arguments and calls this private native transport member, which the shim
    // consumes and deletes. The `net` allowlist is captured at install time
    // (the per-call context does not expose it) and gates every request.
    let capabilities = runtime.capabilities().clone();
    let fetch_call: Arc<RuntimeNativeFn> = Arc::new(move |ctx, args, _captures| {
        crate::fetch_ext::native_fetch(ctx, args, &capabilities)
    });
    runtime.install_native_global_call(
        "__nativeFetch",
        5,
        RuntimeNativeCall::Dynamic(fetch_call),
    )?;
    runtime.install_native_global("__otterStreamCodec", 3, stream_codec)?;
    install_navigator(runtime)?;
    install_self(runtime)?;
    install_promise_rejection_handling(runtime)?;
    Ok(())
}

/// Install the global unhandled-rejection surface (HTML
/// §handler-onunhandledrejection).
///
/// Otter's global object is not a Window/Worker EventTarget, so the
/// `unhandledrejection` / `rejectionhandled` / `error` handler IDL attributes
/// are exposed as plain settable ([Replaceable]-equivalent) globals rather than
/// through `addEventListener`. They and the VM-invoked reporter are installed
/// **eagerly** — the on* attributes must be assignable before any Web global is
/// touched (and a later lazy materialization of `web_bootstrap.js` must not
/// clobber a user-set handler), and the reporter must exist whenever the VM's
/// HostPromiseRejectionTracker checkpoint runs. The reporter references
/// `PromiseRejectionEvent` and `reportError` lazily, so those stay in the
/// deferred `web_bootstrap.js` group and materialize the first time the reporter
/// actually fires.
fn install_promise_rejection_handling(
    runtime: &mut RuntimeExtensionContext<'_>,
) -> Result<(), OtterError> {
    // `handled` is false for the `unhandledrejection` notification and true for
    // the follow-up `rejectionhandled`. Defensive throughout: a throwing
    // handler must never abort the microtask drain the VM invokes this from.
    let shim = "\
        (function (g) {\n\
          function replaceable(name) {\n\
            Object.defineProperty(g, name, {\n\
              value: null, writable: true, enumerable: false, configurable: true,\n\
            });\n\
          }\n\
          replaceable('onunhandledrejection');\n\
          replaceable('onrejectionhandled');\n\
          replaceable('onerror');\n\
          Object.defineProperty(g, '__otterFirePromiseRejection', {\n\
            value: function (promise, reason, handled) {\n\
              var type = handled ? 'rejectionhandled' : 'unhandledrejection';\n\
              var event;\n\
              try {\n\
                event = new g.PromiseRejectionEvent(type, {\n\
                  promise: promise, reason: reason, cancelable: !handled,\n\
                });\n\
              } catch (_) { return; }\n\
              var handler = g['on' + type];\n\
              if (typeof handler === 'function') {\n\
                try { handler.call(g, event); }\n\
                catch (e) { try { g.reportError(e); } catch (_) {} }\n\
              }\n\
              if (!handled && !event.defaultPrevented) {\n\
                try { g.reportError(reason); } catch (_) {}\n\
              }\n\
            },\n\
            writable: true, enumerable: false, configurable: true,\n\
          });\n\
        })(globalThis);";
    runtime
        .install_script(SourceInput::from_javascript(shim))
        .map_err(|err| OtterError::Internal {
            code: "WEB_REJECTION_INSTALL".to_string(),
            message: format!("promise-rejection surface install failed: {err}"),
        })?;
    Ok(())
}

/// Install the `self` global (HTML §dom-self). Otter has no Window/Worker
/// split, so `self` always resolves to `globalThis`. Modelled as a replaceable
/// accessor (`[Replaceable]`): reading returns the global object, and assigning
/// shadows it with a data property, matching platform semantics. Installed
/// eagerly (not lazily) so `self` is present before any Web class is touched.
fn install_self(runtime: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    let shim = "Object.defineProperty(globalThis, 'self', {\n\
          get() { return globalThis; },\n\
          set(value) {\n\
            Object.defineProperty(globalThis, 'self', {\n\
              value, writable: true, enumerable: true, configurable: true,\n\
            });\n\
          },\n\
          enumerable: true,\n\
          configurable: true,\n\
        });";
    runtime
        .install_script(SourceInput::from_javascript(shim))
        .map_err(|err| OtterError::Internal {
            code: "WEB_SELF_INSTALL".to_string(),
            message: format!("self install failed: {err}"),
        })?;
    Ok(())
}

/// Install the `navigator` global (WinterTC Minimum Common API): a plain
/// object exposing `userAgent` with the engine name and crate version.
/// Defined writable + configurable to match the WebIDL `[Replaceable]`
/// attribute shape, and non-enumerable like the other Web globals.
fn install_navigator(runtime: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    let shim = format!(
        "Object.defineProperty(globalThis, 'navigator', {{\n\
           value: {{ userAgent: 'Otter/{version}' }},\n\
           writable: true,\n\
           enumerable: false,\n\
           configurable: true,\n\
         }});",
        version = env!("CARGO_PKG_VERSION"),
    );
    runtime
        .install_script(SourceInput::from_javascript(shim))
        .map_err(|err| OtterError::Internal {
            code: "WEB_NAVIGATOR_INSTALL".to_string(),
            message: format!("navigator install failed: {err}"),
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

/// `queueMicrotask(callback)` — HTML §8.7. Direct bare-identifier calls
/// compile to the VM microtask opcode; this native body serves indirect
/// calls (an aliased or reflected `queueMicrotask`) by enqueueing on the
/// same per-isolate queue. Throws `TypeError` for non-callable arguments.
fn queue_microtask(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let callback = args.first().copied().unwrap_or_else(Value::undefined);
    ctx.queue_microtask(callback, [])?;
    Ok(Value::undefined())
}

fn structured_clone(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    let options = args.get(1).copied().unwrap_or_else(Value::undefined);
    otter_runtime::web_structured_clone::structured_clone_with_options(ctx, value, options)
}

/// Native deflate/gzip codec backing `CompressionStream`/`DecompressionStream`.
/// Args: `(format: string, data: Uint8Array|ArrayBuffer, decompress: boolean)`;
/// returns a `Uint8Array`.
fn stream_codec(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    use std::io::{Read, Write};

    let format = runtime_arg_to_string(args, 0, ctx.heap());
    let decompress = args.get(2).and_then(|v| v.as_boolean()).unwrap_or(false);
    let data = args.get(1).copied().unwrap_or_else(Value::undefined);
    let input: Vec<u8> = if let Some(ta) = data.as_typed_array(ctx.heap()) {
        let off = ta.byte_offset(ctx.heap());
        let len = ta.byte_length(ctx.heap());
        ta.buffer(ctx.heap())
            .with_bytes(ctx.heap(), |b| b.get(off..off + len).map(<[u8]>::to_vec))
            .unwrap_or_default()
    } else if let Some(buf) = data.as_array_buffer() {
        buf.with_bytes(ctx.heap(), |b| b.to_vec())
    } else {
        return Err(runtime_type_error(
            "CompressionStream",
            "chunk must be a BufferSource",
        ));
    };

    let out = if decompress {
        let mut buf = Vec::new();
        let res = match format.as_str() {
            "gzip" => flate2::read::GzDecoder::new(&input[..]).read_to_end(&mut buf),
            "deflate" => flate2::read::ZlibDecoder::new(&input[..]).read_to_end(&mut buf),
            "deflate-raw" => flate2::read::DeflateDecoder::new(&input[..]).read_to_end(&mut buf),
            other => {
                return Err(runtime_type_error(
                    "DecompressionStream",
                    format!("unsupported format '{other}'"),
                ));
            }
        };
        res.map_err(|e| runtime_type_error("DecompressionStream", e.to_string()))?;
        buf
    } else {
        let level = flate2::Compression::default();
        let res: std::io::Result<Vec<u8>> = match format.as_str() {
            "gzip" => {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), level);
                e.write_all(&input).and_then(|()| e.finish())
            }
            "deflate" => {
                let mut e = flate2::write::ZlibEncoder::new(Vec::new(), level);
                e.write_all(&input).and_then(|()| e.finish())
            }
            "deflate-raw" => {
                let mut e = flate2::write::DeflateEncoder::new(Vec::new(), level);
                e.write_all(&input).and_then(|()| e.finish())
            }
            other => {
                return Err(runtime_type_error(
                    "CompressionStream",
                    format!("unsupported format '{other}'"),
                ));
            }
        };
        res.map_err(|e| runtime_type_error("CompressionStream", e.to_string()))?
    };

    let buffer = ctx
        .array_buffer_from_bytes(out)
        .map_err(|e| runtime_type_error("CompressionStream", e.to_string()))?;
    let ctor = ctx
        .global_value("Uint8Array")
        .ok_or_else(|| runtime_type_error("CompressionStream", "Uint8Array is unavailable"))?;
    ctx.construct(ctor, &[Value::array_buffer(buffer)])
}

#[cfg(test)]
mod tests {
    use super::{WEB_BOOTSTRAP, WEB_FETCH, WEB_STREAMS, WEB_URLPATTERN};
    use std::collections::BTreeSet;

    /// Scan a shim source for the `def('<name>')` calls that attach a global.
    /// This is a literal substring scan (not a JS parse) used purely to keep
    /// [`WEB_GLOBAL_NAMES`] in lockstep with the shim sources.
    fn def_names(src: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let bytes = src.as_bytes();
        let needle = b"def('";
        let mut i = 0;
        while i + needle.len() < bytes.len() {
            if &bytes[i..i + needle.len()] == needle {
                let start = i + needle.len();
                if let Some(end_rel) = src[start..].find('\'') {
                    out.insert(src[start..start + end_rel].to_string());
                }
                i = start;
            } else {
                i += 1;
            }
        }
        out
    }

    /// The romp! declaration's `defines` lists must match the
    /// `def('…')` globals each shim source actually installs — the
    /// build-time honesty check for declaration-derived lazy names.
    #[test]
    fn lazy_global_names_match_shim_def_calls() {
        let mut from_shims = def_names(WEB_BOOTSTRAP);
        from_shims.extend(def_names(WEB_STREAMS));
        from_shims.extend(def_names(WEB_FETCH));
        from_shims.extend(def_names(WEB_URLPATTERN));
        let declared: BTreeSet<String> = crate::WEB_EXTENSION
            .lazy_names()
            .map(str::to_string)
            .collect();
        assert_eq!(
            from_shims, declared,
            "romp! `defines` lists must match the def('...') globals installed by the shim sources"
        );
    }
}
