//! Hyper HTTP service implementation
//!
//! Handles incoming HTTP requests and routes them to JavaScript handlers.

use crate::http::request::{HttpRequest, HttpResponse, insert_request};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::Service;
use hyper::{Request, Response, StatusCode};
use otter_vm_runtime::HttpEvent;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{mpsc, oneshot};

/// HTTP service for hyper
#[derive(Clone)]
pub struct OtterHttpService {
    server_id: u64,
    event_tx: mpsc::UnboundedSender<HttpEvent>,
    peer_addr: Option<SocketAddr>,
    is_tls: bool,
    pending_requests: Arc<AtomicU64>,
}

impl OtterHttpService {
    /// Create a new HTTP service
    pub fn new(
        server_id: u64,
        event_tx: mpsc::UnboundedSender<HttpEvent>,
        peer_addr: Option<SocketAddr>,
        is_tls: bool,
        pending_requests: Arc<AtomicU64>,
    ) -> Self {
        Self {
            server_id,
            event_tx,
            peer_addr,
            is_tls,
            pending_requests,
        }
    }
}

impl Service<Request<Incoming>> for OtterHttpService {
    type Response = Response<Full<Bytes>>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let server_id = self.server_id;
        let event_tx = self.event_tx.clone();
        let peer_addr = self.peer_addr;
        let is_tls = self.is_tls;
        let pending_requests = Arc::clone(&self.pending_requests);

        Box::pin(async move {
            let mut req = req;
            let upgrade = hyper::upgrade::on(&mut req);
            let (parts, body) = req.into_parts();

            let scheme = if is_tls { "https" } else { "http" };
            let path = parts.uri.to_string();
            let full_url = if path.starts_with("http://") || path.starts_with("https://") {
                path
            } else {
                let host = parts
                    .headers
                    .get("host")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("localhost");
                format!("{}://{}{}", scheme, host, path)
            };

            let (response_tx, response_rx) = oneshot::channel::<HttpResponse>();

            let request_id = insert_request(HttpRequest {
                method: parts.method.as_str().to_string(),
                url: full_url,
                headers: parts.headers,
                body: Some(body),
                peer_addr,
                upgrade: Some(upgrade),
                pending_requests,
                response_tx,
            });

            let _ = event_tx.send(HttpEvent {
                server_id,
                request_id,
            });

            match tokio::time::timeout(std::time::Duration::from_secs(30), response_rx).await {
                Ok(Ok(http_response)) => {
                    let mut response = Response::builder().status(
                        StatusCode::from_u16(http_response.status).unwrap_or(StatusCode::OK),
                    );

                    for (key, value) in http_response.headers {
                        response = response.header(key, value);
                    }

                    Ok(response
                        .body(Full::new(Bytes::from(http_response.body)))
                        .unwrap_or_else(|_| {
                            Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Full::new(Bytes::from("Internal Server Error")))
                                .unwrap()
                        }))
                }
                Ok(Err(_)) => Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from("Handler did not respond")))
                    .unwrap()),
                Err(_) => Ok(Response::builder()
                    .status(StatusCode::GATEWAY_TIMEOUT)
                    .body(Full::new(Bytes::from("Request timeout")))
                    .unwrap()),
            }
        })
    }
}
