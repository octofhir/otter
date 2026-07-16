//! Typed construction of JS values from Rust returns.
//!
//! [`IntoJs`] is the construction direction of the marshalling layer:
//! a binding body returns plain Rust data, and the glue converts it
//! into a scope handle with one trait call. Byte-carrying returns are
//! explicit newtypes ([`ArrayBuffer`], [`Uint8Array`]) so the JS shape
//! of a byte payload is visible at the declaration site rather than
//! inferred from a `Vec<u8>`.
//!
//! # Contents
//! - [`IntoJs`] — the construction trait.
//! - [`ArrayBuffer`] / [`Uint8Array`] — byte-payload newtypes.
//!
//! # Invariants
//! - Conversions only mint values through the ambient scope; the
//!   returned handle is parked and safe across later allocations.
//! - `None` converts to `null` (WebIDL nullable), not `undefined`.
//!
//! # See also
//! - [`super::from_js`] — the extraction direction.

use crate::handles::Local;

use super::cx::MarshalCx;
use super::error::JsError;
use super::from_js::{DOMString, USVString};

/// Convert `self` into a JS value parked in the ambient scope.
pub trait IntoJs {
    /// Perform the conversion.
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError>;
}

impl IntoJs for () {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.undefined())
    }
}

impl IntoJs for bool {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.boolean(self))
    }
}

impl IntoJs for f64 {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(self))
    }
}

impl IntoJs for i32 {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(f64::from(self)))
    }
}

impl IntoJs for u32 {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(f64::from(self)))
    }
}

/// Converts through `f64`; values above 2^53 lose precision, matching
/// what a JS number can hold.
impl IntoJs for u64 {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(self as f64))
    }
}

/// Converts through `f64`; values above 2^53 lose precision, matching
/// what a JS number can hold.
impl IntoJs for i64 {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(self as f64))
    }
}

/// Converts through `f64`; values above 2^53 lose precision, matching
/// what a JS number can hold.
impl IntoJs for usize {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        Ok(cx.number(self as f64))
    }
}

impl IntoJs for &str {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.string(self)
    }
}

impl IntoJs for String {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.string(&self)
    }
}

impl IntoJs for DOMString {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.string_from_units(self.as_units())
    }
}

impl IntoJs for USVString {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.string(self.as_str())
    }
}

/// WebIDL nullable: `None` becomes `null`.
impl<T: IntoJs> IntoJs for Option<T> {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        match self {
            Some(value) => value.into_js(cx),
            None => Ok(cx.null()),
        }
    }
}

impl<T: IntoJs> IntoJs for Vec<T> {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        let array = cx.array(self.len())?;
        for (index, element) in self.into_iter().enumerate() {
            let element = element.into_js(cx)?;
            cx.set_index(array, index, element)?;
        }
        Ok(array)
    }
}

/// Re-park an existing handle (identity conversion across scopes).
impl IntoJs for Local<'_> {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        let raw = cx.escape(self);
        Ok(cx.park(raw))
    }
}

/// Byte payload returned as a JS `ArrayBuffer`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArrayBuffer(pub Vec<u8>);

impl IntoJs for ArrayBuffer {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.array_buffer_from_bytes(self.0)
    }
}

/// Byte payload returned as a JS `Uint8Array` over a fresh buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uint8Array(pub Vec<u8>);

impl IntoJs for Uint8Array {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Local<'s>, JsError> {
        cx.uint8_array_from_bytes(self.0)
    }
}
