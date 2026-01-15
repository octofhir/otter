//! Compile-fail tests for thread safety
//!
//! These tests verify that JscContext and JscValue cannot be sent across threads.
//! The `compile_fail` doc tests ensure that attempting to send these types
//! to another thread results in a compilation error.

/// ```compile_fail
/// use otter_jsc_core::JscContext;
/// use std::thread;
///
/// let ctx = JscContext::new().unwrap();
/// thread::spawn(move || {
///     // This should fail to compile: JscContext is !Send
///     let _ = ctx.eval("1 + 1");
/// });
/// ```
fn _context_not_send() {}

/// ```compile_fail
/// use otter_jsc_core::JscContext;
/// use std::sync::Arc;
///
/// let ctx = Arc::new(JscContext::new().unwrap());
/// let ctx2 = ctx.clone();
/// std::thread::spawn(move || {
///     // This should fail to compile: JscContext is !Sync
///     let _ = ctx2;
/// });
/// ```
fn _context_not_sync() {}

/// ```compile_fail
/// use otter_jsc_core::JscContext;
/// use std::thread;
///
/// let ctx = JscContext::new().unwrap();
/// let value = ctx.eval("42").unwrap();
/// thread::spawn(move || {
///     // This should fail to compile: JscValue is !Send
///     let _ = value.to_number();
/// });
/// ```
fn _value_not_send() {}

/// ```compile_fail
/// use otter_jsc_core::JscString;
/// use std::thread;
///
/// let s = JscString::new("hello").unwrap();
/// thread::spawn(move || {
///     // This should fail to compile: JscString is !Send
///     let _ = s.to_string();
/// });
/// ```
fn _string_not_send() {}

/// ```compile_fail
/// use otter_jsc_core::{JscContext, JscObject};
/// use std::thread;
///
/// let ctx = JscContext::new().unwrap();
/// let obj = JscObject::empty(ctx.raw());
/// thread::spawn(move || {
///     // This should fail to compile: JscObject is !Send
///     let _ = obj.is_array();
/// });
/// ```
fn _object_not_send() {}
