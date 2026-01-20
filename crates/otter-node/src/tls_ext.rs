//! TLS extension module for Node.js compatibility.
//!
//! This module provides TLS/SSL encrypted TCP connections using rustls.

use crate::net::NetEvent;
use crate::tls::{init_tls_manager, ActiveTlsServerCount};
use otter_runtime::Extension;
use std::cell::RefCell;
use tokio::sync::mpsc;

thread_local! {
    static ACTIVE_COUNT: RefCell<Option<ActiveTlsServerCount>> = RefCell::new(None);
}

/// Get active TLS server/count for keep-alive tracking.
pub fn get_active_count() -> ActiveTlsServerCount {
    ACTIVE_COUNT.with(|count| {
        count
            .borrow()
            .clone()
            .expect("TLS active count not initialized")
    })
}

/// Initialize TLS module with event channel.
pub fn init(event_tx: mpsc::UnboundedSender<NetEvent>) -> ActiveTlsServerCount {
    let active_count = init_tls_manager(event_tx);

    ACTIVE_COUNT.with(|count| {
        *count.borrow_mut() = Some(active_count.clone());
    });

    active_count
}

/// Create TLS extension.
pub fn extension() -> Extension {
    let js_code = include_str!("tls.js");

    Extension::new("tls")
        .with_ops(vec![
            crate::tls::tls_connect_dive_decl(),
            crate::tls::tls_socket_write_dive_decl(),
            crate::tls::tls_socket_write_string_dive_decl(),
            crate::tls::tls_socket_end_dive_decl(),
            crate::tls::tls_socket_destroy_dive_decl(),
        ])
        .with_js(js_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "tls");
        assert!(ext.js_code().is_some());
    }
}
