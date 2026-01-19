//! Node.js https module extension
//!
//! Registers the https module which provides TLS-encrypted HTTP client and server.

use otter_runtime::Extension;

/// Create the https extension
pub fn extension() -> Extension {
    Extension::new("node_https").with_js(include_str!("https.js"))
}
