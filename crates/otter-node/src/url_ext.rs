//! URL extension module using the new architecture.
//!
//! This module provides the node:url extension with WHATWG URL Standard
//! compliant URL and URLSearchParams classes.
//!
//! ## Architecture
//!
//! - `url.rs` - Rust URL parsing implementation
//! - `url_ext.rs` - Extension creation with #[dive] ops
//! - `url_shim.js` - JavaScript URL/URLSearchParams classes

use otter_macros::dive;
use otter_runtime::Extension;
use serde_json::json;

use crate::url::UrlComponents;

/// Parse a URL string, optionally with a base URL.
///
/// Returns URL components on success, or an error object on failure.
#[dive(swift)]
fn __otter_url_parse(url_string: String, base: Option<String>) -> serde_json::Value {
    match UrlComponents::parse(&url_string, base.as_deref()) {
        Ok(components) => serde_json::to_value(components).unwrap_or(json!(null)),
        Err(e) => json!({ "error": e }),
    }
}

/// Set a URL component and return updated components.
///
/// Takes the current href, component name, and new value.
/// Returns updated URL components on success, or an error object on failure.
#[dive(swift)]
fn __otter_url_set_component(href: String, component: String, value: String) -> serde_json::Value {
    match UrlComponents::parse(&href, None) {
        Ok(components) => match components.set_component(&component, &value) {
            Ok(updated) => serde_json::to_value(updated).unwrap_or(json!(null)),
            Err(e) => json!({ "error": e }),
        },
        Err(e) => json!({ "error": e }),
    }
}

/// Create the url extension.
///
/// This extension provides WHATWG URL Standard compliant URL and URLSearchParams
/// classes using native Rust parsing via the `url` crate.
pub fn extension() -> Extension {
    Extension::new("url")
        .with_ops(vec![
            __otter_url_parse_dive_decl(),
            __otter_url_set_component_dive_decl(),
        ])
        .with_js(include_str!("url_shim.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "url");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_url_parse_op() {
        let result = __otter_url_parse("https://example.com/path".to_string(), None);
        assert!(result.get("href").is_some());
        assert_eq!(result["protocol"], "https:");
    }

    #[test]
    fn test_url_parse_with_base() {
        let result =
            __otter_url_parse("/path".to_string(), Some("https://example.com".to_string()));
        assert_eq!(result["href"], "https://example.com/path");
    }

    #[test]
    fn test_url_set_component() {
        let result = __otter_url_set_component(
            "https://example.com/old".to_string(),
            "pathname".to_string(),
            "/new".to_string(),
        );
        assert_eq!(result["pathname"], "/new");
    }
}
