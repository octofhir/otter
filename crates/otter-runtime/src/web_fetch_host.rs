//! Outbound HTTP transport backing the Web `fetch()` global.
//!
//! `otter-web` owns the `fetch()` JS contract and the `Request`/`Response`
//! classes; this module owns the network side that the plan places in
//! `otter-runtime`: the HTTP request, the `allow-net` capability
//! gate, and the plain-data request/response DTOs exchanged across the crate
//! boundary. No VM handles cross this boundary — only owned, `Send` data.
//!
//! # Contents
//! - [`FetchRequest`] / [`FetchResponse`] — plain-data DTOs.
//! - [`FetchTransport`] — reusable, capability-aware HTTP client pair.
//! - [`perform_fetch`] — capability-gated async request.
//!
//! # Invariants
//! - Outbound network is deny-by-default: [`perform_fetch`] checks the initial
//!   URL and the redirect policy checks every next URL before its socket is
//!   opened.
//! - A transport reuses its clients and connection pools across calls; policy
//!   state is immutable and safe to capture in async work.
//! - Errors are stringly-typed for the JS boundary; the shim maps them to the
//!   spec `TypeError` a rejected `fetch()` promise carries.
//!
//! # See also
//! - <https://fetch.spec.whatwg.org/>

use std::error::Error as _;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::RuntimeCapabilityEvaluator;

/// Cancellation handle for an in-flight [`prepare_fetch`] request. Calling
/// [`FetchAbort::abort`] cancels the request (closing the socket by dropping the
/// request future); dropping the handle without aborting lets it run normally.
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

/// Reusable HTTP transport bound to one immutable runtime capability policy.
///
/// Separate clients preserve Fetch's followed and non-followed redirect modes
/// while sharing nothing mutable with the VM. A client opens its connector on
/// its first request, so making a transport costs no TLS or socket setup.
#[derive(Clone)]
pub struct FetchTransport {
    inner: Arc<FetchTransportInner>,
}

struct FetchTransportInner {
    evaluator: RuntimeCapabilityEvaluator,
    follow_client: otter_http_client::Client,
    no_redirect_client: otter_http_client::Client,
}

impl std::fmt::Debug for FetchTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchTransport")
            .field("evaluator", &self.inner.evaluator)
            .field("follow_client", &self.inner.follow_client)
            .field("no_redirect_client", &self.inner.no_redirect_client)
            .finish()
    }
}

impl FetchTransport {
    /// Bind a reusable HTTP transport to one runtime policy snapshot.
    #[must_use]
    pub fn new(evaluator: RuntimeCapabilityEvaluator, user_agent: String) -> Self {
        let check_evaluator = evaluator.clone();
        let check: otter_http_client::RedirectCheck = Arc::new(move |next, previous| {
            if check_evaluator.check_network(next, Some(previous)) {
                Ok(())
            } else {
                Err(format!(
                    "network access to \"{next}\" is not allowed; grant it with --allow-net"
                ))
            }
        });
        let follow_client = otter_http_client::Client::builder()
            .user_agent(&user_agent)
            .redirects(otter_http_client::Redirects::Follow(Some(check)))
            .build();
        let no_redirect_client = otter_http_client::Client::builder()
            .user_agent(&user_agent)
            .redirects(otter_http_client::Redirects::Stop)
            .build();
        Self {
            inner: Arc::new(FetchTransportInner {
                evaluator,
                follow_client,
                no_redirect_client,
            }),
        }
    }

    fn client(&self, follow_redirects: bool) -> &otter_http_client::Client {
        if follow_redirects {
            &self.inner.follow_client
        } else {
            &self.inner.no_redirect_client
        }
    }
}

/// Build a cancellable outbound fetch. Returns the abort handle and the future
/// to drive on the host executor; the future resolves with the response, or
/// errors with `"fetch aborted"` if [`FetchAbort::abort`] fires first. The
/// capability gate lives in [`perform_fetch`], so a refused host still rejects.
pub fn prepare_fetch(
    request: FetchRequest,
    transport: FetchTransport,
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
            result = perform_fetch(request, transport) => result,
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
    /// Whether a redirect was followed to reach [`Self::final_url`].
    pub redirected: bool,
}

/// The streaming body of a fetched response. Each [`ResponseBody::pull`] reads
/// the next chunk directly off the connection, so the transport applies natural
/// backpressure — bytes are only pulled from the socket when the reader asks.
/// The `tokio` mutex serializes pulls (the `ReadableStream` protocol pulls one
/// chunk at a time) and is held across the `await`.
pub struct ResponseBody {
    response: tokio::sync::Mutex<Option<otter_http_client::Response>>,
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
/// The initial URL and every followed redirect target must pass the transport's
/// capability evaluator before a connection is made. The body is left on the
/// connection and streamed lazily through [`ResponseBody::pull`].
pub async fn perform_fetch(
    request: FetchRequest,
    transport: FetchTransport,
) -> Result<(FetchResponseHead, ResponseBody), String> {
    let parsed =
        otter_http_client::Url::parse(&request.url).map_err(|err| format!("invalid URL: {err}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("URL has no host: {}", request.url))?;
    if !transport.inner.evaluator.check_network(&parsed, None) {
        return Err(format!(
            "network access to \"{host}\" is not allowed; grant it with --allow-net"
        ));
    }

    let method = otter_http_client::Method::from_bytes(request.method.as_bytes())
        .map_err(|_| format!("invalid HTTP method: {}", request.method))?;
    // `error` and `manual` both stop redirects: `error` rejects below when a
    // 3xx is seen, while `manual` hands the response back to the caller.
    let client = transport.client(request.redirect == "follow");
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
        .map_err(|error| format_transport_error("fetch failed", &error))?;
    if request.redirect == "error" && response.status().is_redirection() {
        return Err(format!(
            "fetch failed: redirect to \"{}\" refused (redirect mode is \"error\")",
            response
                .headers()
                .get(otter_http_client::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
        ));
    }
    let status = response.status();
    let head = FetchResponseHead {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or("").to_string(),
        final_url: response.url().to_string(),
        redirected: response.redirected(),
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

fn format_transport_error(prefix: &str, error: &otter_http_client::Error) -> String {
    let mut message = format!("{prefix}: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}
