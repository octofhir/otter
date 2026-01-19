//! Node.js Stream extension module.
//!
//! Provides the node:stream module with full Node.js streams API:
//! - `Stream` - base stream class (extends EventEmitter)
//! - `Readable` - readable stream with read(), push(), pipe(), etc.
//! - `Writable` - writable stream with write(), end(), cork/uncork
//! - `Duplex` - both readable and writable
//! - `Transform` - duplex with _transform() method
//! - `PassThrough` - transform that passes data unchanged
//!
//! Also provides utility functions:
//! - `pipeline()` - connect streams with error handling
//! - `finished()` - wait for stream completion
//! - `compose()` - compose multiple streams
//! - `addAbortSignal()` - add abort support
//!
//! The implementation is pure JavaScript, using the existing EventEmitter
//! from the events module.

use otter_runtime::Extension;

/// Create the Node.js stream extension.
///
/// This extension provides the full Node.js streams API including:
/// - All stream classes (Stream, Readable, Writable, Duplex, Transform, PassThrough)
/// - Static methods like Readable.from(), Readable.fromWeb(), Readable.toWeb()
/// - Utility functions (pipeline, finished, compose, addAbortSignal)
/// - stream/promises sub-module
///
/// The implementation uses the existing EventEmitter from globalThis.__EventEmitter.
pub fn extension() -> Extension {
    Extension::new("node_stream").with_js(include_str!("node_stream.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "node_stream");
        assert!(ext.js_code().is_some());
    }
}
