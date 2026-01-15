//! Node.js compatibility layer for Otter.
//!
//! This crate provides Node.js-compatible APIs for the Otter runtime.
//!
//! # Modules
//!
//! - `path` - Path manipulation utilities (no capabilities required)
//! - `buffer` - Binary data handling
//! - `fs` - File system operations (requires capabilities)
//! - `test` - Test runner (describe, it, assert)
//! - `extensions` - JavaScript extensions for runtime integration
//!
//! # Example
//!
//! ```no_run
//! use otter_node::path;
//! use otter_node::buffer::Buffer;
//!
//! // Path manipulation
//! let joined = path::join(&["foo", "bar", "baz.txt"]);
//! assert_eq!(joined, "foo/bar/baz.txt");
//!
//! // Buffer operations
//! let buf = Buffer::from_string("hello", "utf8").unwrap();
//! assert_eq!(buf.to_string("base64", 0, buf.len()), "aGVsbG8=");
//! ```

pub mod buffer;
pub mod extensions;
pub mod fs;
pub mod path;
pub mod test;

pub use buffer::{Buffer, BufferError};
pub use extensions::{
    create_buffer_extension, create_fs_extension, create_path_extension, create_test_extension,
};
pub use fs::{FsError, ReadResult, Stats};
pub use path::ParsedPath;
pub use test::{TestResult, TestRunner, TestRunnerHandle, TestSummary};

// Re-export capabilities for convenience
pub use otter_engine::Capabilities;
