//! HTTP request storage
//!
//! Thread-safe storage for pending HTTP requests.
//! Uses DashMap for concurrent access.

use dashmap::DashMap;
use http::HeaderMap;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;

/// Global request storage
static REQUESTS: OnceLock<DashMap<u64, HttpRequest>> = OnceLock::new();
/// Next request ID counter
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn get_storage() -> &'static DashMap<u64, HttpRequest> {
    REQUESTS.get_or_init(DashMap::new)
}

/// HTTP request data
pub struct HttpRequest {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Full URL
    pub url: String,
    /// Request headers
    pub headers: HeaderMap,
    /// Request body stream (consumed on read)
    pub body: Option<Incoming>,
    /// Client peer address
    pub peer_addr: Option<SocketAddr>,
    /// Upgrade handle for WebSocket (set for all requests)
    pub upgrade: Option<OnUpgrade>,
    /// Pending request counter for the owning server
    pub pending_requests: Arc<AtomicU64>,
    /// Channel to send response back to hyper service
    pub response_tx: oneshot::Sender<HttpResponse>,
}

// Incoming is Send, but we store it behind DashMap + oneshot.
// SAFETY: Access is synchronized via DashMap entry guards.
unsafe impl Send for HttpRequest {}

/// HTTP response data
pub struct HttpResponse {
    /// Status code
    pub status: u16,
    /// Response headers
    pub headers: HashMap<String, String>,
    /// Response body bytes
    pub body: Vec<u8>,
}

/// Insert a new request and return its ID
pub fn insert_request(req: HttpRequest) -> u64 {
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    req.pending_requests.fetch_add(1, Ordering::Relaxed);
    get_storage().insert(id, req);
    id
}

/// Get a reference to a request by ID
pub fn get_request(id: u64) -> Option<dashmap::mapref::one::Ref<'static, u64, HttpRequest>> {
    get_storage().get(&id)
}

/// Remove and return a request by ID
pub fn remove_request(id: u64) -> Option<HttpRequest> {
    let removed = get_storage().remove(&id).map(|(_, v)| v);
    if let Some(ref req) = removed {
        req.pending_requests.fetch_sub(1, Ordering::Relaxed);
    }
    removed
}

/// Get the number of pending requests
pub fn pending_count() -> usize {
    get_storage().len()
}

/// Basic metadata for lazy access
#[derive(serde::Serialize)]
pub struct BasicMetadata {
    pub method: String,
    pub url: String,
}

/// Get basic metadata (method + url)
pub fn get_basic_metadata(id: u64) -> Option<BasicMetadata> {
    get_request(id).map(|req| BasicMetadata {
        method: req.method.clone(),
        url: req.url.clone(),
    })
}

/// Get all headers for a request as a HashMap
pub fn get_request_headers(id: u64) -> Option<HashMap<String, String>> {
    get_request(id).map(|req| header_map_to_hashmap(&req.headers))
}

/// Get a peer address for a request
pub fn get_request_peer_addr(id: u64) -> Option<SocketAddr> {
    get_request(id).and_then(|req| req.peer_addr)
}

/// Read the request body as bytes (async). Consumes the body.
pub async fn read_request_body(id: u64) -> Option<Vec<u8>> {
    let body = get_storage()
        .get_mut(&id)
        .and_then(|mut req| req.body.take())?;

    match body.collect().await {
        Ok(collected) => Some(collected.to_bytes().to_vec()),
        Err(_) => None,
    }
}

/// Send a response for a request.
pub fn send_response(
    id: u64,
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> bool {
    let Some((_, req)) = get_storage().remove(&id) else {
        return false;
    };

    req.pending_requests.fetch_sub(1, Ordering::Relaxed);

    let response = HttpResponse {
        status,
        headers,
        body,
    };

    req.response_tx.send(response).is_ok()
}

/// Send text response (fast path)
pub fn send_text_response(id: u64, status: u16, body: &str) -> bool {
    let mut headers = HashMap::new();
    headers.insert(
        "content-type".to_string(),
        "text/plain; charset=utf-8".to_string(),
    );
    send_response(id, status, headers, body.as_bytes().to_vec())
}

fn header_map_to_hashmap(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            map.insert(name.as_str().to_string(), v.to_string());
        }
    }
    map
}
