//! Safe wrapper around JSC values with automatic GC protection
//!
//! Re-exports from jsc-core for compatibility.

pub use jsc_core::string::js_string_to_rust;
pub use jsc_core::{extract_exception, JscValue};
