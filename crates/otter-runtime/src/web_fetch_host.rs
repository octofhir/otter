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

use crate::Permission;

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
}

/// Plain-data response handed back to the `fetch` shim, which mints a
/// `Response` from it.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// HTTP status code.
    pub status: u16,
    /// Reason phrase (canonical for the status when the server omits one).
    pub status_text: String,
    /// Response header name/value pairs in wire order.
    pub headers: Vec<(String, String)>,
    /// Fully buffered response body.
    pub body: Vec<u8>,
    /// Final URL after redirects.
    pub final_url: String,
}

/// Perform a buffered outbound HTTP request, gated by the `net` capability.
///
/// The URL's `host[:port]` must match `net` or the request is refused before a
/// connection is made. Redirects follow reqwest's default policy; the request
/// and response bodies are fully buffered (streaming is a later slice).
pub async fn perform_fetch(
    request: FetchRequest,
    user_agent: String,
    net: Permission<String>,
) -> Result<FetchResponse, String> {
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
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
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
    let status = response.status();
    let status_text = status.canonical_reason().unwrap_or("").to_string();
    let final_url = response.url().to_string();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|err| format!("fetch body read failed: {err}"))?
        .to_vec();
    Ok(FetchResponse {
        status: status.as_u16(),
        status_text,
        headers,
        body,
        final_url,
    })
}
