//! Node.js http2 module extension (stub)
//!
//! HTTP/2 is not fully implemented. This is a compatibility stub.

use otter_runtime::Extension;

/// Create the http2 extension (stub for compatibility)
pub fn extension() -> Extension {
    Extension::new("node_http2").with_js(include_str!("http2.js"))
}
