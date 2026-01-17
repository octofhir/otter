//! HTTP request storage and operations for Otter.serve()
//!
//! Provides thread-safe storage for pending HTTP requests and operations
//! to access request data from JavaScript without JSON serialization overhead.
//!
//! Uses DashMap for fine-grained locking - each request entry is locked
//! independently, minimizing contention under high load.

use crate::http_service::{OtterBody, full_body};
use bytes::Bytes;
use dashmap::DashMap;
use http::{HeaderMap, Method, Uri};
use http_body_util::BodyExt;
use hyper::Response;
use hyper::body::Incoming;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::oneshot;

/// A stored HTTP request awaiting JavaScript handler processing.
pub struct HttpRequest {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Option<Incoming>,
    pub response_tx: Option<oneshot::Sender<Response<OtterBody>>>,
}

// Incoming and oneshot::Sender are both Send, so HttpRequest is Send
// SAFETY: We only access HttpRequest from synchronized code
unsafe impl Send for HttpRequest {}

/// Global counter for unique request IDs.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    /// Concurrent storage for pending HTTP requests.
    /// Uses DashMap for fine-grained locking per entry.
    pub static ref REQUEST_STORE: DashMap<u64, HttpRequest> = DashMap::new();
}

/// Insert a request into the store and return its ID.
/// Uses atomic ID generation - no global lock needed.
pub fn insert_request(request: HttpRequest) -> u64 {
    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    REQUEST_STORE.insert(id, request);
    id
}

/// Remove a request from the store.
pub fn remove_request(request_id: u64) -> Option<HttpRequest> {
    REQUEST_STORE.remove(&request_id).map(|(_, req)| req)
}

/// Check if a request exists.
pub fn request_exists(request_id: u64) -> bool {
    REQUEST_STORE.contains_key(&request_id)
}

/// Get number of pending requests.
pub fn pending_request_count() -> usize {
    REQUEST_STORE.len()
}

/// Get the HTTP method for a request.
pub fn get_request_method(request_id: u64) -> Option<String> {
    REQUEST_STORE
        .get(&request_id)
        .map(|req| req.method.as_str().to_string())
}

/// Get the full URL for a request.
pub fn get_request_url(request_id: u64) -> Option<String> {
    REQUEST_STORE
        .get(&request_id)
        .map(|req| req.uri.to_string())
}

/// Get a specific header value for a request.
pub fn get_request_header(request_id: u64, name: &str) -> Option<String> {
    REQUEST_STORE.get(&request_id).and_then(|req| {
        req.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    })
}

/// Get all headers for a request as a HashMap.
pub fn get_request_headers(request_id: u64) -> Option<HashMap<String, String>> {
    REQUEST_STORE.get(&request_id).map(|req| {
        let mut headers = HashMap::new();
        for (name, value) in req.headers.iter() {
            if let Ok(v) = value.to_str() {
                headers.insert(name.as_str().to_string(), v.to_string());
            }
        }
        headers
    })
}

/// Request metadata for batch access (reduces lock acquisitions).
#[derive(serde::Serialize)]
pub struct RequestMetadata {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
}

/// Get all request metadata in a single lock acquisition.
pub fn get_request_metadata(request_id: u64) -> Option<RequestMetadata> {
    REQUEST_STORE.get(&request_id).map(|req| {
        let mut headers = HashMap::new();
        for (name, value) in req.headers.iter() {
            if let Ok(v) = value.to_str() {
                headers.insert(name.as_str().to_string(), v.to_string());
            }
        }

        RequestMetadata {
            method: req.method.as_str().to_string(),
            url: req.uri.to_string(),
            headers,
        }
    })
}

/// Basic metadata (method + full url) - faster than full metadata.
/// Used for lazy headers optimization.
/// Includes fully constructed URL so JS doesn't need to load headers for Host.
#[derive(serde::Serialize)]
pub struct BasicMetadata {
    pub method: String,
    pub url: String,
}

/// Get basic metadata (method + full url) without headers.
/// Constructs full URL including host, so JS doesn't need headers.
pub fn get_basic_metadata(request_id: u64) -> Option<BasicMetadata> {
    REQUEST_STORE.get(&request_id).map(|req| {
        // Construct full URL from Host header + path
        let path = req.uri.to_string();
        let full_url = if path.starts_with("http://") || path.starts_with("https://") {
            path
        } else {
            // Get host from headers
            let host = req
                .headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            format!("http://{}{}", host, path)
        };

        BasicMetadata {
            method: req.method.as_str().to_string(),
            url: full_url,
        }
    })
}

/// Get only headers for a request (for lazy loading).
pub fn get_request_headers_only(request_id: u64) -> Option<HashMap<String, String>> {
    REQUEST_STORE.get(&request_id).map(|req| {
        let mut headers = HashMap::new();
        for (name, value) in req.headers.iter() {
            if let Ok(v) = value.to_str() {
                headers.insert(name.as_str().to_string(), v.to_string());
            }
        }
        headers
    })
}

/// Read the request body as bytes (async).
/// This consumes the body - can only be called once per request.
pub async fn read_request_body(request_id: u64) -> Option<Vec<u8>> {
    // Take the body out of the request
    let body = REQUEST_STORE
        .get_mut(&request_id)
        .and_then(|mut req| req.body.take())?;

    // Read body to bytes
    match body.collect().await {
        Ok(collected) => Some(collected.to_bytes().to_vec()),
        Err(e) => {
            tracing::warn!(request_id, error = %e, "Failed to read request body");
            None
        }
    }
}

/// Send a response for a request.
/// Returns true if response was sent successfully.
pub fn send_response(
    request_id: u64,
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> bool {
    // Remove atomically - single operation
    let Some((_, mut req)) = REQUEST_STORE.remove(&request_id) else {
        return false;
    };

    let Some(tx) = req.response_tx.take() else {
        return false;
    };

    // Build response WITHOUT any locks held
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    let response = builder
        .body(full_body(Bytes::from(body)))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(500)
                .body(full_body("Failed to build response"))
                .unwrap()
        });

    tx.send(response).is_ok()
}

/// Send text response directly - no HashMap allocation.
/// Fast path for simple text responses.
pub fn send_text_response_direct(request_id: u64, status: u16, body: &str) -> bool {
    let Some((_, mut req)) = REQUEST_STORE.remove(&request_id) else {
        return false;
    };

    let Some(tx) = req.response_tx.take() else {
        return false;
    };

    let response = Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full_body(Bytes::from(body.to_owned())))
        .unwrap();

    tx.send(response).is_ok()
}

/// Send a simple text response (legacy wrapper).
pub fn send_text_response(request_id: u64, status: u16, body: &str) -> bool {
    send_text_response_direct(request_id, status, body)
}

/// Send a JSON response.
pub fn send_json_response(request_id: u64, status: u16, body: &str) -> bool {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    send_response(request_id, status, headers, body.as_bytes().to_vec())
}

/// Cancel a request (send 500 error).
pub fn cancel_request(request_id: u64) {
    send_text_response(request_id, 500, "Request cancelled");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_store_basic() {
        let id = insert_request(HttpRequest {
            method: Method::GET,
            uri: "/test".parse().unwrap(),
            headers: HeaderMap::new(),
            body: None,
            response_tx: None,
        });

        assert!(request_exists(id));
        assert_eq!(get_request_method(id), Some("GET".to_string()));

        remove_request(id);
        assert!(!request_exists(id));
    }

    #[test]
    fn test_basic_metadata() {
        let id = insert_request(HttpRequest {
            method: Method::POST,
            uri: "/api/test".parse().unwrap(),
            headers: HeaderMap::new(),
            body: None,
            response_tx: None,
        });

        let meta = get_basic_metadata(id).unwrap();
        assert_eq!(meta.method, "POST");
        assert_eq!(meta.url, "http://localhost/api/test");

        remove_request(id);
    }
}
