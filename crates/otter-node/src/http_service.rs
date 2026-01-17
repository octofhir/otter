//! Hyper Service implementation for Otter HTTP server.
//!
//! This service handles incoming HTTP requests, stores them in thread-safe storage,
//! and sends events to the worker thread for JavaScript handler invocation.

use crate::http_request::{HttpRequest, insert_request, remove_request};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};
use otter_runtime::HttpEvent;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// HTTP response body type used by Otter.
pub type OtterBody = BoxBody<Bytes, Infallible>;

/// Create an empty response body.
pub fn empty_body() -> OtterBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// Create a response body from bytes.
pub fn full_body(data: impl Into<Bytes>) -> OtterBody {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

/// Create a 500 Internal Server Error response.
pub fn error_500() -> Response<OtterBody> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(full_body("Internal Server Error"))
        .unwrap()
}

/// Hyper service for handling HTTP requests in Otter.
#[derive(Clone)]
pub struct OtterHttpService {
    server_id: u64,
    event_tx: UnboundedSender<HttpEvent>,
}

impl OtterHttpService {
    /// Create a new service instance.
    pub fn new(server_id: u64, event_tx: UnboundedSender<HttpEvent>) -> Self {
        Self {
            server_id,
            event_tx,
        }
    }
}

impl Service<Request<Incoming>> for OtterHttpService {
    type Response = Response<OtterBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let server_id = self.server_id;
        let event_tx = self.event_tx.clone();

        Box::pin(async move {
            // Create channel for response
            let (response_tx, response_rx) = oneshot::channel::<Response<OtterBody>>();

            // Extract request parts
            let (parts, body) = req.into_parts();

            // Store the request and get an ID (thread-safe)
            let request_id = insert_request(HttpRequest {
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
                body: Some(body),
                response_tx: Some(response_tx),
            });

            // Send event to worker (this wakes it up immediately via crossbeam Select)
            if let Err(e) = event_tx.send(HttpEvent {
                server_id,
                request_id,
            }) {
                tracing::error!(
                    server_id,
                    request_id,
                    error = %e,
                    "Failed to send HTTP event to worker"
                );
                // Clean up the stored request
                remove_request(request_id);
                return Ok(error_500());
            }

            // Wait for response from JavaScript handler
            match response_rx.await {
                Ok(response) => Ok(response),
                Err(_) => {
                    // Handler dropped without responding
                    tracing::warn!(
                        server_id,
                        request_id,
                        "Request handler dropped without responding"
                    );
                    Ok(error_500())
                }
            }
        })
    }
}
