//! Node.js extensions module - clean API for creating extensions.
//!
//! This module provides a clean API for accessing Node.js-compatible extensions.
//!
//! ## Usage
//!
//! ```rust,ignore
//! use otter_node::ext;
//!
//! // Simple extensions (no dependencies)
//! let exts = vec![
//!     ext::path(),
//!     ext::events(),
//!     ext::util(),
//!     ext::url(),
//!     ext::os(),
//!     ext::test(),
//! ];
//!
//! // Extensions with dependencies
//! let exts = vec![
//!     ext::buffer(),
//!     ext::fs(capabilities),
//!     ext::net(),
//!     ext::http_server(event_tx),
//! ];
//! ```
//!
//! ## Extension Categories
//!
//! Extensions are organized by their requirements:
//!
//! ### Core (no dependencies, safe for embedded)
//! - `path()` - Path manipulation
//! - `events()` - EventEmitter
//! - `util()` - Utility functions
//! - `url()` - URL parsing
//! - `os()` - OS information
//!
//! ### IO (may require capabilities)
//! - `buffer()` - Binary data handling
//! - `fs(capabilities)` - File system operations
//! - `process()` - Process information
//! - `child_process()` - Child process spawning
//! - `streams()` - Web Streams API
//!
//! ### Network (may be disabled in embedded)
//! - `net()` - TCP/UDP networking
//! - `http()` - HTTP client
//! - `http_server(event_tx)` - HTTP server
//! - `websocket()` - WebSocket client
//!
//! ### Runtime
//! - `crypto()` - Cryptographic operations
//! - `worker()` - Web Workers
//! - `test()` - Test runner

use otter_runtime::Extension;

// ============================================================================
// Core Extensions (safe for embedded, no dependencies)
// ============================================================================

/// Create the path extension for path manipulation utilities.
///
/// This is a core extension safe for embedded use.
pub fn path() -> Extension {
    crate::path_ext::create_path_extension()
}

/// Create the events extension (EventEmitter).
///
/// This is a core extension safe for embedded use.
pub fn events() -> Extension {
    crate::events_ext::extension()
}

/// Create the util extension for utility functions.
///
/// Provides `util.promisify`, `util.format`, `util.inspect`.
/// This is a core extension safe for embedded use.
pub fn util() -> Extension {
    crate::util_ext::extension()
}

/// Create the url extension for WHATWG URL parsing.
///
/// Provides URL and URLSearchParams classes.
/// This is a core extension safe for embedded use.
pub fn url() -> Extension {
    crate::url_ext::extension()
}

/// Create the os extension for operating system utilities.
///
/// This is a core extension safe for embedded use.
/// Note: OS information is collected at extension creation time.
pub fn os() -> Extension {
    crate::os_ext::extension()
}

/// Create the test extension for test runner functionality.
///
/// Provides describe, it, test, and assertion APIs.
pub fn test() -> Extension {
    crate::test_ext::extension()
}

/// Create the assert extension for assertion utilities.
///
/// Provides Node.js-compatible assert API (node:assert).
pub fn assert() -> Extension {
    crate::assert_ext::extension()
}

/// Create the async_hooks extension for async context tracking.
///
/// Provides Node.js-compatible async_hooks API (node:async_hooks).
/// This is a stub implementation that provides the API surface
/// needed for Express dependencies to work.
pub fn async_hooks() -> Extension {
    crate::async_hooks_ext::extension()
}

/// Create the querystring extension for query string parsing.
///
/// Provides Node.js-compatible querystring API (node:querystring).
pub fn querystring() -> Extension {
    crate::querystring_ext::extension()
}

/// Create the dns extension for DNS resolution.
///
/// Provides Node.js-compatible dns API (node:dns).
pub fn dns() -> Extension {
    crate::dns_ext::extension()
}

/// Create the dgram extension for UDP sockets.
///
/// Provides Node.js-compatible dgram API (node:dgram).
pub fn dgram() -> Extension {
    crate::dgram_ext::extension()
}

/// Create the string_decoder extension for buffer to string decoding.
///
/// Provides StringDecoder class for handling multi-byte sequences across chunks.
pub fn string_decoder() -> Extension {
    crate::string_decoder_ext::extension()
}

/// Create the readline extension for line-by-line input reading.
///
/// Provides readline.createInterface() for CLI applications.
pub fn readline() -> Extension {
    crate::readline_ext::extension()
}

/// Create the Node.js stream extension.
///
/// Provides Node.js-compatible stream classes (Readable, Writable, Duplex, Transform, PassThrough)
/// and utility functions (pipeline, finished, compose).
pub fn node_stream() -> Extension {
    crate::node_stream_ext::extension()
}

// ============================================================================
// IO Extensions (may require capabilities)
// ============================================================================

/// Create the buffer extension for binary data handling.
///
/// Provides Node.js-compatible Buffer class.
pub fn buffer() -> Extension {
    crate::buffer_ext::extension()
}

/// Create the crypto extension for cryptographic operations.
///
/// Provides hash, hmac, random bytes, and other crypto functions.
pub fn crypto() -> Extension {
    crate::crypto_ext::extension()
}

/// Create the zlib extension for compression/decompression.
///
/// Provides gzip, deflate, and brotli compression algorithms.
pub fn zlib() -> Extension {
    crate::zlib_ext::extension()
}

/// Create the process extension for process information.
///
/// Provides process.memoryUsage and other process utilities.
pub fn process() -> Extension {
    crate::process_ext::extension()
}

/// Create the HTTP extension for HTTP client functionality.
///
/// Provides http.request and http.get methods.
pub fn http() -> Extension {
    crate::http_ext::extension()
}

/// Create the HTTPS extension for HTTPS client functionality.
///
/// Provides https.request and https.get methods (TLS-encrypted HTTP).
pub fn https() -> Extension {
    crate::https_ext::extension()
}

/// Create the HTTP/2 extension (stub for compatibility).
///
/// HTTP/2 is not fully implemented; this provides basic exports for compatibility.
pub fn http2() -> Extension {
    crate::http2_ext::extension()
}

/// Create the TTY extension for terminal detection.
///
/// Provides isatty, ReadStream, and WriteStream.
pub fn tty() -> Extension {
    crate::tty_ext::extension()
}

// ============================================================================
// Complex Extensions (require dependencies or shared state)
// ============================================================================

/// Create the fs extension for file system operations.
///
/// Requires capabilities to control read/write access.
pub fn fs(capabilities: crate::Capabilities) -> Extension {
    crate::fs_ext::extension(capabilities)
}

/// Create the websocket extension for WebSocket connections.
///
/// Provides Web-standard WebSocket API.
pub fn websocket() -> Extension {
    crate::websocket_ext::extension()
}

/// Create the worker extension for Web Workers.
///
/// Provides Web Worker API for running JavaScript in background threads.
pub fn worker() -> Extension {
    crate::worker_ext::extension()
}

/// Create the streams extension for Web Streams API.
///
/// Provides ReadableStream, WritableStream, and TransformStream.
pub fn streams() -> Extension {
    crate::streams_ext::extension()
}

/// Create the child_process extension for spawning processes.
///
/// Provides Node.js-compatible child process spawning.
pub fn child_process() -> Extension {
    crate::child_process_ext::extension()
}

/// Create the http_server extension for HTTP servers.
///
/// Returns a tuple of (Extension, ActiveServerCount) so the runtime can track
/// when all servers have stopped.
pub fn http_server(
    event_tx: tokio::sync::mpsc::UnboundedSender<crate::http_server::HttpEvent>,
) -> (Extension, crate::http_server::ActiveServerCount) {
    crate::http_server_ext::extension(event_tx)
}

/// Create the net extension for TCP/UDP networking.
///
/// Provides Node.js-compatible net module for low-level networking.
pub fn net() -> Extension {
    crate::net::create_net_extension()
}

/// Create the process_ipc extension for inter-process communication.
///
/// Provides IPC channel support for parent-child process communication.
/// Only available on Unix platforms.
#[cfg(unix)]
pub fn process_ipc(ipc_channel: crate::ipc::IpcChannel) -> Extension {
    crate::process_ipc_ext::extension(ipc_channel)
}

// ============================================================================
// Preset-based Extension Registration
// ============================================================================

pub use otter_runtime::{ExtensionKind, ExtensionPreset};

/// Get extensions for embedded environments (safe, no IO/network).
///
/// Returns: path, buffer, util, events, url, crypto
///
/// These extensions are safe for use in sandboxed/embedded contexts
/// where IO and network access should be restricted.
///
/// # Example
///
/// ```rust,ignore
/// use otter_node::ext;
///
/// for extension in ext::for_embedded() {
///     runtime.register_extension(extension)?;
/// }
/// ```
pub fn for_embedded() -> Vec<Extension> {
    vec![path(), buffer(), util(), events(), url(), crypto(), zlib()]
}

/// Get extensions for a specific preset.
///
/// For presets that require capabilities (NodeCompat, Full), use `for_preset_with_config` instead.
pub fn for_preset(preset: ExtensionPreset) -> Vec<Extension> {
    match preset {
        ExtensionPreset::Embedded => for_embedded(),
        // For NodeCompat and Full, return embedded set as fallback
        // Use for_preset_with_config for full functionality
        _ => for_embedded(),
    }
}

/// Configuration for creating extensions with dependencies.
pub struct ExtensionConfig {
    /// Capabilities for file system access
    pub capabilities: crate::Capabilities,
    /// HTTP event sender for Otter.serve()
    pub http_event_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::http_server::HttpEvent>>,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            capabilities: crate::Capabilities::none(),
            http_event_tx: None,
        }
    }
}

impl ExtensionConfig {
    /// Create config with all permissions.
    pub fn with_all_permissions() -> Self {
        Self {
            capabilities: crate::Capabilities::all(),
            http_event_tx: None,
        }
    }

    /// Set capabilities.
    pub fn capabilities(mut self, caps: crate::Capabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Set HTTP event sender for server support.
    pub fn http_event_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::http_server::HttpEvent>,
    ) -> Self {
        self.http_event_tx = Some(tx);
        self
    }
}

/// Result of creating extensions with config.
pub struct ExtensionsWithState {
    /// The extensions to register
    pub extensions: Vec<Extension>,
    /// HTTP server active count (if http_server was created)
    pub http_server_count: Option<crate::http_server::ActiveServerCount>,
}

/// Get extensions for Node.js compatibility with full config.
///
/// This includes all extensions needed for Node.js compatibility:
/// - Core: path, buffer, util, events, url, crypto, os
/// - IO: fs, process, child_process, streams
/// - Network: http, websocket, worker
/// - Server: http_server (if http_event_tx provided)
///
/// # Example
///
/// ```rust,ignore
/// use otter_node::ext::{self, ExtensionConfig};
///
/// let (http_tx, http_rx) = tokio::sync::mpsc::unbounded_channel();
/// let config = ExtensionConfig::with_all_permissions()
///     .http_event_tx(http_tx);
///
/// let result = ext::for_node_compat(config);
/// for extension in result.extensions {
///     runtime.register_extension(extension)?;
/// }
/// ```
pub fn for_node_compat(config: ExtensionConfig) -> ExtensionsWithState {
    let mut extensions = vec![
        // Core
        path(),
        buffer(),
        util(),
        events(),
        url(),
        crypto(),
        zlib(),
        assert(),
        querystring(),
        dns(),
        dgram(),
        string_decoder(),
        readline(),
        os(),
        async_hooks(),
        // IO
        fs(config.capabilities.clone()),
        process(),
        child_process(),
        streams(),
        node_stream(),
        // Network
        http(),
        websocket(),
        worker(),
    ];

    let mut http_server_count = None;

    // Add http_server if tx is provided
    if let Some(tx) = config.http_event_tx {
        let (ext, count) = http_server(tx);
        extensions.push(ext);
        http_server_count = Some(count);
    }

    ExtensionsWithState {
        extensions,
        http_server_count,
    }
}

/// Get all extensions (for Full preset).
///
/// Same as `for_node_compat` but also includes test extension.
pub fn for_full(config: ExtensionConfig) -> ExtensionsWithState {
    let mut result = for_node_compat(config);
    result.extensions.push(test());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_extension() {
        let ext = path();
        assert_eq!(ext.name(), "path");
    }

    #[test]
    fn test_events_extension() {
        let ext = events();
        assert_eq!(ext.name(), "events");
    }

    #[test]
    fn test_util_extension() {
        let ext = util();
        assert_eq!(ext.name(), "util");
    }

    #[test]
    fn test_url_extension() {
        let ext = url();
        assert_eq!(ext.name(), "url");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_os_extension() {
        let ext = os();
        assert_eq!(ext.name(), "os");
    }

    #[test]
    fn test_test_extension() {
        let ext = test();
        assert_eq!(ext.name(), "test");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_buffer_extension() {
        let ext = buffer();
        assert_eq!(ext.name(), "Buffer");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_crypto_extension() {
        let ext = crypto();
        assert_eq!(ext.name(), "crypto");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_process_extension() {
        let ext = process();
        assert_eq!(ext.name(), "process");
    }

    #[test]
    fn test_zlib_extension() {
        let ext = zlib();
        assert_eq!(ext.name(), "zlib");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_assert_extension() {
        let ext = assert();
        assert_eq!(ext.name(), "assert");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_async_hooks_extension() {
        let ext = async_hooks();
        assert_eq!(ext.name(), "async_hooks");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_querystring_extension() {
        let ext = querystring();
        assert_eq!(ext.name(), "querystring");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_dns_extension() {
        let ext = dns();
        assert_eq!(ext.name(), "dns");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_dgram_extension() {
        let ext = dgram();
        assert_eq!(ext.name(), "dgram");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_http_extension() {
        let ext = http();
        assert_eq!(ext.name(), "http");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_fs_extension() {
        use crate::Capabilities;
        let ext = fs(Capabilities::none());
        assert_eq!(ext.name(), "fs");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_websocket_extension() {
        let ext = websocket();
        assert_eq!(ext.name(), "WebSocket");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_worker_extension() {
        let ext = worker();
        assert_eq!(ext.name(), "Worker");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_streams_extension() {
        let ext = streams();
        assert_eq!(ext.name(), "Streams");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_child_process_extension() {
        let ext = child_process();
        assert_eq!(ext.name(), "child_process");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_node_stream_extension() {
        let ext = node_stream();
        assert_eq!(ext.name(), "node_stream");
        assert!(ext.js_code().is_some());
    }

    #[test]
    fn test_http_server_extension() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ext, _count) = http_server(tx);
        assert_eq!(ext.name(), "http_server");
        assert!(ext.js_code().is_some());
    }

    // ====== Preset-based extension tests ======

    #[test]
    fn test_for_embedded() {
        let extensions = for_embedded();
        // Should have 7 extensions: path, buffer, util, events, url, crypto, zlib
        assert_eq!(extensions.len(), 7);

        let names: Vec<&str> = extensions.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"path"));
        assert!(names.contains(&"Buffer"));
        assert!(names.contains(&"util"));
        assert!(names.contains(&"events"));
        assert!(names.contains(&"url"));
        assert!(names.contains(&"crypto"));
        assert!(names.contains(&"zlib"));

        // Should NOT have IO/network extensions
        assert!(!names.contains(&"fs"));
        assert!(!names.contains(&"http"));
        assert!(!names.contains(&"WebSocket"));
    }

    #[test]
    fn test_for_preset_embedded() {
        let extensions = for_preset(ExtensionPreset::Embedded);
        assert_eq!(extensions.len(), 7);
    }

    #[test]
    fn test_extension_config_default() {
        let config = ExtensionConfig::default();
        // Default should have no permissions
        assert!(config.http_event_tx.is_none());
    }

    #[test]
    fn test_extension_config_with_all_permissions() {
        let config = ExtensionConfig::with_all_permissions();
        assert!(config.http_event_tx.is_none());
    }

    #[test]
    fn test_extension_config_builder() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ExtensionConfig::default()
            .capabilities(crate::Capabilities::all())
            .http_event_tx(tx);

        assert!(config.http_event_tx.is_some());
    }

    #[test]
    fn test_for_node_compat_without_http_server() {
        let config = ExtensionConfig::with_all_permissions();
        let result = for_node_compat(config);

        // Should have 23 extensions (no http_server since no tx provided)
        assert_eq!(result.extensions.len(), 23);
        assert!(result.http_server_count.is_none());

        let names: Vec<&str> = result.extensions.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"path"));
        assert!(names.contains(&"Buffer"));
        assert!(names.contains(&"fs"));
        assert!(names.contains(&"http"));
        assert!(names.contains(&"WebSocket"));
        assert!(names.contains(&"Worker"));
        assert!(names.contains(&"zlib"));
        assert!(names.contains(&"assert"));
        assert!(names.contains(&"querystring"));
        assert!(names.contains(&"dns"));
        assert!(names.contains(&"dgram"));
        assert!(names.contains(&"string_decoder"));
        assert!(names.contains(&"readline"));
        assert!(names.contains(&"node_stream"));
        // No http_server without tx
        assert!(!names.contains(&"http_server"));
    }

    #[test]
    fn test_for_node_compat_with_http_server() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ExtensionConfig::with_all_permissions().http_event_tx(tx);
        let result = for_node_compat(config);

        // Should have 24 extensions (including http_server)
        assert_eq!(result.extensions.len(), 24);
        assert!(result.http_server_count.is_some());

        let names: Vec<&str> = result.extensions.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"http_server"));
    }

    #[test]
    fn test_for_full_includes_test() {
        let config = ExtensionConfig::with_all_permissions();
        let result = for_full(config);

        // Should have 24 extensions (node_compat 23 + test)
        assert_eq!(result.extensions.len(), 24);

        let names: Vec<&str> = result.extensions.iter().map(|e| e.name()).collect();
        assert!(names.contains(&"test"));
    }

    #[test]
    fn test_for_full_with_http_server() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ExtensionConfig::with_all_permissions().http_event_tx(tx);
        let result = for_full(config);

        // Should have 25 extensions (all)
        assert_eq!(result.extensions.len(), 25);
        assert!(result.http_server_count.is_some());
    }
}
