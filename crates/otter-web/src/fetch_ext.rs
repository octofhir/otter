//! Native backing for the Web `fetch()` global.
//!
//! `fetch.js` owns the spec contract: it normalizes `(input, init)` into a
//! `Request`, extracts the method / absolute URL / flattened headers / buffered
//! body, and calls the private [`native_fetch`] member with those plain values.
//! This native gates the request on the `net` capability, drives the reqwest
//! transport in [`otter_runtime::web_fetch_host`] off-thread through the async
//! completion protocol, and resolves the promise with the raw response parts
//! (`[status, statusText, flatHeaders, bodyBytes]`) the shim turns into a
//! `Response`. Consumed and deleted by the shim so no hidden hook remains.
//!
//! # Contents
//! - [`native_fetch`] — the private `__nativeFetch` member.
//!
//! # Invariants
//! - Network is deny-by-default: the capability check lives in
//!   [`otter_runtime::web_fetch_host::perform_fetch`], and a refusal rejects the
//!   returned promise with a `TypeError` (fetch never throws synchronously).
//! - No VM handle escapes into the spawned future — only owned, `Send` data.
//!
//! # See also
//! - <https://fetch.spec.whatwg.org/#fetch-method>

use otter_runtime::marshal::{IntoJs, JsError, MarshalCx};
use otter_runtime::web_fetch_host::{FetchRequest, FetchResponse, prepare_fetch};
use otter_runtime::{
    CapabilitySet, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeScoped as Scoped, RuntimeValue as Value,
};

/// The buffered fetch result, marshalled to the `[status, statusText,
/// flatHeaders, bodyBytes, finalUrl]` array the `fetch.js` shim mints a
/// `Response` from.
struct NativeFetchResult(FetchResponse);

impl IntoJs for NativeFetchResult {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Scoped<'s>, JsError> {
        let response = self.0;
        let array = cx.array(5)?;
        let status = cx.number(f64::from(response.status));
        cx.set_index(array, 0, status)?;
        let status_text = cx.string(&response.status_text)?;
        cx.set_index(array, 1, status_text)?;
        let mut flat = Vec::with_capacity(response.headers.len() * 2);
        for (name, value) in response.headers {
            flat.push(name);
            flat.push(value);
        }
        let flat = flat.into_js(cx)?;
        cx.set_index(array, 2, flat)?;
        let body = cx.uint8_array_from_bytes(response.body)?;
        cx.set_index(array, 3, body)?;
        let final_url = cx.string(&response.final_url)?;
        cx.set_index(array, 4, final_url)?;
        Ok(array)
    }
}

/// `__nativeFetch(method, url, flatHeaders, body)` — the private compute member.
/// `method`/`url` are strings, `flatHeaders` is `[name0, value0, …]` (names
/// pre-lowercased), and `body` is a `Uint8Array` or `null`/`undefined`. Returns
/// `{ promise, abort }`: `promise` resolves to the response parts array, and
/// `abort()` cancels the in-flight request (closing the socket). The shim races
/// `abort` against an `AbortSignal` to implement `fetch`'s cancellation.
pub fn native_fetch(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    caps: &CapabilitySet,
) -> Result<Value, NativeError> {
    let arg = |index: usize| args.get(index).copied().unwrap_or_else(Value::undefined);
    let net = caps.net.clone();
    let user_agent = format!("Otter/{}", env!("CARGO_PKG_VERSION"));

    ctx.scope(|ctx, scope| {
        let mut cx = MarshalCx::new(ctx, scope);

        let method_handle = cx.park(arg(0));
        let method = cx.as_string_lossy(method_handle).unwrap_or_default();
        let url_handle = cx.park(arg(1));
        let url = cx.as_string_lossy(url_handle).unwrap_or_default();

        let headers_handle = cx.park(arg(2));
        let header_handles = cx
            .iterate_to_handles(headers_handle)
            .map_err(|err| err.into_native("fetch"))?;
        let mut headers = Vec::with_capacity(header_handles.len() / 2);
        for pair in header_handles.chunks_exact(2) {
            let name = cx.as_string_lossy(pair[0]).unwrap_or_default();
            let value = cx.as_string_lossy(pair[1]).unwrap_or_default();
            headers.push((name, value));
        }

        let body_handle = cx.park(arg(3));
        let body = cx.buffer_source_bytes(body_handle);

        let request = FetchRequest {
            method,
            url,
            headers,
            body,
        };
        let (abort, transport) = prepare_fetch(request, user_agent, net);
        let future = async move {
            transport
                .await
                .map(NativeFetchResult)
                .map_err(JsError::Type)
        };
        let promise = cx
            .promise_from_future(future)
            .map_err(|err| err.into_native("fetch"))?;

        // `abort()` cancels the in-flight request; idempotent, so the shim can
        // wire it to a one-shot `AbortSignal` listener without guarding.
        let abort_fn = cx
            .ctx()
            .native_value(
                "fetch.abort",
                Default::default(),
                move |_ctx, _args, _captures| {
                    abort.abort();
                    Ok(Value::undefined())
                },
            )
            .map_err(|err| JsError::Type(err.to_string()).into_native("fetch"))?;
        let abort_handle = cx.park(abort_fn);

        let result = cx.object().map_err(|err| err.into_native("fetch"))?;
        cx.set(result, "promise", promise)
            .map_err(|err| err.into_native("fetch"))?;
        cx.set(result, "abort", abort_handle)
            .map_err(|err| err.into_native("fetch"))?;
        Ok(cx.escape(result))
    })
}
