//! Outbound HTTP transport backing the Web `fetch()` global.
//!
//! `otter-web` owns the `fetch()` JS contract and the `Request`/`Response`
//! classes; this module owns the network side that the plan places in
//! `otter-runtime`: the reqwest-backed request, the `allow-net` capability
//! gate, and the plain-data request/response DTOs exchanged across the crate
//! boundary. No VM handles cross this boundary — only owned, `Send` data.
//!
//! # Contents
//! - [`FetchRequest`] / [`FetchResponse`] — plain-data DTOs.
//! - [`perform_fetch`] — capability-gated async request.
//!
//! # Invariants
//! - Outbound network is deny-by-default: [`perform_fetch`] rejects any host
//!   the [`Permission`] allowlist does not match before a socket is opened.
//! - Errors are stringly-typed for the JS boundary; the shim maps them to the
//!   spec `TypeError` a rejected `fetch()` promise carries.
//!
//! # See also
//! - <https://fetch.spec.whatwg.org/>

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::Permission;

/// Cancellation handle for an in-flight [`prepare_fetch`] request. Calling
/// [`FetchAbort::abort`] cancels the request (closing the socket by dropping the
/// reqwest future); dropping the handle without aborting lets it run normally.
/// `Send + Sync` so it can be captured by the JS-visible abort callback.
pub struct FetchAbort {
    sender: Mutex<Option<oneshot::Sender<()>>>,
}

impl FetchAbort {
    /// Cancel the in-flight request. Idempotent: later calls are no-ops.
    pub fn abort(&self) {
        if let Some(sender) = self
            .sender
            .lock()
            .expect("fetch abort mutex poisoned")
            .take()
        {
            let _ = sender.send(());
        }
    }
}

/// Build a cancellable outbound fetch. Returns the abort handle and the future
/// to drive on the host executor; the future resolves with the response, or
/// errors with `"fetch aborted"` if [`FetchAbort::abort`] fires first. The
/// capability gate lives in [`perform_fetch`], so a refused host still rejects.
pub fn prepare_fetch(
    request: FetchRequest,
    user_agent: String,
    net: Permission<String>,
) -> (
    Arc<FetchAbort>,
    impl Future<Output = Result<(FetchResponseHead, ResponseBody), String>> + Send,
) {
    let (sender, receiver) = oneshot::channel();
    let abort = Arc::new(FetchAbort {
        sender: Mutex::new(Some(sender)),
    });
    let future = async move {
        tokio::select! {
            result = perform_fetch(request, user_agent, net) => result,
            _ = receiver => Err("fetch aborted".to_string()),
        }
    };
    (abort, future)
}

/// Plain-data outbound request assembled by the `fetch` shim from a normalized
/// `Request` (method, absolute URL, pre-flattened headers, buffered body).
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// HTTP method, upper-cased by the shim.
    pub method: String,
    /// Absolute request URL.
    pub url: String,
    /// Header name/value pairs (names already lower-cased by the shim).
    pub headers: Vec<(String, String)>,
    /// Buffered request body, or `None` for bodiless methods.
    pub body: Option<Vec<u8>>,
    /// Redirect handling mode: `"follow"` (default), `"error"`, or `"manual"`.
    pub redirect: String,
}

/// Plain-data response head handed back to the `fetch` shim, which mints a
/// `Response` from it. The body is streamed separately through [`ResponseBody`].
#[derive(Debug, Clone)]
pub struct FetchResponseHead {
    /// HTTP status code.
    pub status: u16,
    /// Reason phrase (canonical for the status when the server omits one).
    pub status_text: String,
    /// Response header name/value pairs in wire order.
    pub headers: Vec<(String, String)>,
    /// Final URL after redirects.
    pub final_url: String,
}

/// The streaming body of a fetched response. Each [`ResponseBody::pull`] reads
/// the next chunk directly off the connection, so the transport applies natural
/// backpressure — bytes are only pulled from the socket when the reader asks.
/// The `tokio` mutex serializes pulls (the `ReadableStream` protocol pulls one
/// chunk at a time) and is held across the `await`.
pub struct ResponseBody {
    response: tokio::sync::Mutex<Option<reqwest::Response>>,
}

impl ResponseBody {
    /// Read the next body chunk. `Ok(Some)` is a chunk, `Ok(None)` is
    /// end-of-stream, `Err` is a transport failure. Idempotent once drained.
    pub async fn pull(&self) -> Result<Option<Vec<u8>>, String> {
        let mut guard = self.response.lock().await;
        let Some(response) = guard.as_mut() else {
            return Ok(None);
        };
        match response.chunk().await {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => {
                *guard = None;
                Ok(None)
            }
            Err(err) => {
                *guard = None;
                Err(format!("fetch body read failed: {err}"))
            }
        }
    }
}

/// Perform an outbound HTTP request, gated by the `net` capability, and return
/// the response head plus a streaming [`ResponseBody`].
///
/// The URL's `host[:port]` must match `net` or the request is refused before a
/// connection is made. Redirects follow reqwest's default policy. The body is
/// left on the connection and streamed lazily through [`ResponseBody::pull`].
pub async fn perform_fetch(
    request: FetchRequest,
    user_agent: String,
    net: Permission<String>,
) -> Result<(FetchResponseHead, ResponseBody), String> {
    let parsed = reqwest::Url::parse(&request.url).map_err(|err| format!("invalid URL: {err}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("URL has no host: {}", request.url))?;
    // `net` patterns are `host[:port]`; accept a match on either the bare host
    // or the host with its effective port so `--allow-net=example.com` and
    // `--allow-net=example.com:443` both work.
    let host_allowed = net.matches(host)
        || parsed
            .port_or_known_default()
            .is_some_and(|port| net.matches(&format!("{host}:{port}")));
    if !host_allowed {
        return Err(format!(
            "network access to \"{host}\" is not allowed; grant it with --allow-net"
        ));
    }

    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid HTTP method: {}", request.method))?;
    // `error` and `manual` both stop reqwest from following: `error` rejects
    // below when a 3xx is seen, `manual` hands the 3xx back to the caller.
    let redirect_policy = match request.redirect.as_str() {
        "error" | "manual" => reqwest::redirect::Policy::none(),
        _ => reqwest::redirect::Policy::default(),
    };
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .redirect(redirect_policy)
        .build()
        .map_err(|err| format!("fetch client init failed: {err}"))?;
    let mut builder = client.request(method, parsed);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }

    let response = builder
        .send()
        .await
        .map_err(|err| format!("fetch failed: {err}"))?;
    if request.redirect == "error" && response.status().is_redirection() {
        return Err(format!(
            "fetch failed: redirect to \"{}\" refused (redirect mode is \"error\")",
            response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
        ));
    }
    let status = response.status();
    let head = FetchResponseHead {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        final_url: response.url().to_string(),
        headers: response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect(),
    };
    let body = ResponseBody {
        response: tokio::sync::Mutex::new(Some(response)),
    };
    Ok((head, body))
}
