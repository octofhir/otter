//! Safe wrapper around JSC values with automatic GC protection
//!
//! Re-exports from jsc-core for compatibility.

pub use otter_jsc_core::string::js_string_to_rust;
pub use otter_jsc_core::{JscValue, extract_exception};
