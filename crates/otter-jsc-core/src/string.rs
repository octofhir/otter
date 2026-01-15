//! RAII wrapper for JSC strings

use otter_jsc_sys::*;
use std::ffi::{CString, c_char};
use std::fmt;
use std::marker::PhantomData;

use crate::error::{JscError, JscResult};

/// RAII wrapper for JSStringRef with automatic release
///
/// # Thread Safety
///
/// This type is `!Send` and `!Sync` because JSC strings should not be
/// shared across threads.
pub struct JscString {
    raw: JSStringRef,
    /// Marker to make this type !Send + !Sync
    _not_send: PhantomData<*mut ()>,
}

impl JscString {
    /// Create a new JSC string from a Rust string
    pub fn new(s: &str) -> JscResult<Self> {
        let c_str = CString::new(s).map_err(|e| JscError::Internal(e.to_string()))?;
        // SAFETY: c_str is valid null-terminated UTF-8
        let raw = unsafe { JSStringCreateWithUTF8CString(c_str.as_ptr()) };
        if raw.is_null() {
            return Err(JscError::Internal("Failed to create JSString".into()));
        }
        Ok(Self {
            raw,
            _not_send: PhantomData,
        })
    }

    /// Get the raw JSStringRef
    pub fn raw(&self) -> JSStringRef {
        self.raw
    }

    /// Convert to Rust String
    fn to_rust_string(&self) -> String {
        // SAFETY: self.raw is valid (checked in new())
        unsafe { js_string_to_rust(self.raw) }
    }

    /// Get the length in UTF-16 code units
    pub fn len(&self) -> usize {
        // SAFETY: self.raw is valid
        unsafe { JSStringGetLength(self.raw) }
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for JscString {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: self.raw was created by JSStringCreateWithUTF8CString
            unsafe { JSStringRelease(self.raw) };
        }
    }
}

impl fmt::Display for JscString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_rust_string())
    }
}

/// Convert JSStringRef to Rust String
///
/// # Safety
/// The js_str must be a valid JSStringRef or null
pub unsafe fn js_string_to_rust(js_str: JSStringRef) -> String {
    if js_str.is_null() {
        return String::new();
    }

    // SAFETY: js_str is valid per caller contract
    unsafe {
        let max_size = JSStringGetMaximumUTF8CStringSize(js_str);
        let mut buffer = vec![0u8; max_size];
        let actual_size =
            JSStringGetUTF8CString(js_str, buffer.as_mut_ptr() as *mut c_char, max_size);

        if actual_size > 0 {
            // actual_size includes null terminator
            buffer.truncate(actual_size - 1);
            String::from_utf8_lossy(&buffer).into_owned()
        } else {
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_creation() {
        let s = JscString::new("hello").unwrap();
        assert_eq!(s.to_string(), "hello");
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn test_empty_string() {
        let s = JscString::new("").unwrap();
        assert!(s.is_empty());
        assert_eq!(s.to_string(), "");
    }

    #[test]
    fn test_unicode_string() {
        let s = JscString::new("hello").unwrap();
        assert_eq!(s.to_string(), "hello");
    }
}
