//! Typed extraction of Rust values from JS arguments.
//!
//! [`FromJs`] is the WebIDL-flavoured conversion trait generated
//! binding glue drives once per argument, left to right. Primitive
//! impls follow the spec coercions (and can therefore re-enter user
//! JS); the wrapper types spell out the extraction shapes Web-style
//! APIs need — strings with and without lone-surrogate scrubbing,
//! iterable sequences, binary buffer views, branded host references,
//! and callable arguments.
//!
//! # Contents
//! - [`FromJs`] — the conversion trait.
//! - [`DOMString`] / [`USVString`] — WTF-16-preserving vs USV strings.
//! - [`Sequence`] — `sequence<T>` over any iterable.
//! - [`BufferSource`] — `ArrayBuffer` or any typed-array view, bytes
//!   copied out at conversion time.
//! - [`HostRef`] — brand-checked reference to a host-data instance.
//! - [`Callback`] — a callable argument, invokable through the context.
//! - [`JsValue`] — the identity escape (a raw scope handle).
//!
//! # Invariants
//! - Every conversion is total over its declared inputs: it returns a
//!   typed value or a [`JsError`] naming the value (via
//!   [`ValueIdent`]) — it never panics on user input.
//! - [`BufferSource`] copies bytes at its own conversion step; a later
//!   argument's re-entrant coercion cannot invalidate it (a detached
//!   buffer reads as empty, mirroring the platform copy semantics).
//! - Integer conversions use the spec modular reductions (`ToInt32` /
//!   `ToUint32`; 64-bit reduction inherits `f64` precision).
//!
//! # See also
//! - [`super::cx`] — the primitives these impls ride.
//! - [`super::into_js`] — the construction direction.

use std::marker::PhantomData;

use crate::handles::Scoped;

use super::cx::MarshalCx;
use super::error::{JsError, ValueIdent};

/// The identity extraction: a raw handle into the ambient scope.
pub type JsValue<'s> = Scoped<'s>;

/// Extract `Self` from the JS value behind `v`, following WebIDL
/// coercion semantics. `ident` names the value in error messages.
pub trait FromJs<'s>: Sized {
    /// Perform the conversion.
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError>;
}

impl<'s> FromJs<'s> for Scoped<'s> {
    fn from_js(
        _cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        _ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        Ok(v)
    }
}

impl<'s> FromJs<'s> for f64 {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.to_number_spec(v)
            .map_err(|err| err.for_ident(ident, "a number"))
    }
}

impl<'s> FromJs<'s> for bool {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        _ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        Ok(cx.to_boolean(v))
    }
}

impl<'s> FromJs<'s> for i32 {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        f64::from_js(cx, v, ident).map(to_int32)
    }
}

impl<'s> FromJs<'s> for u32 {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        f64::from_js(cx, v, ident).map(to_uint32)
    }
}

impl<'s> FromJs<'s> for i64 {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        f64::from_js(cx, v, ident).map(to_int64)
    }
}

/// Nullish (`undefined` / `null`) reads as `None`; anything else runs
/// the inner conversion. This merges WebIDL "optional with default
/// absent" and "nullable" — bindings that must distinguish them read
/// the raw handle.
impl<'s, T: FromJs<'s>> FromJs<'s> for Option<T> {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        if cx.is_nullish(v) {
            return Ok(None);
        }
        T::from_js(cx, v, ident).map(Some)
    }
}

/// §7.1.6 `ToInt32` modular reduction.
#[must_use]
pub fn to_int32(n: f64) -> i32 {
    to_uint32(n) as i32
}

/// §7.1.7 `ToUint32` modular reduction.
#[must_use]
pub fn to_uint32(n: f64) -> u32 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    const TWO_POW_32: f64 = 4_294_967_296.0;
    let m = n.trunc().rem_euclid(TWO_POW_32);
    m as u32
}

/// WebIDL `long long` modular reduction. Inputs beyond 2^53 inherit
/// `f64` precision, as the spec's `ToNumber`-based pipeline does.
#[must_use]
pub fn to_int64(n: f64) -> i64 {
    if !n.is_finite() || n == 0.0 {
        return 0;
    }
    const TWO_POW_64: f64 = 18_446_744_073_709_551_616.0;
    const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
    let m = n.trunc().rem_euclid(TWO_POW_64);
    if m >= TWO_POW_63 {
        (m - TWO_POW_64) as i64
    } else {
        m as i64
    }
}

impl JsError {
    /// Prefix a conversion error with the value's identity.
    #[must_use]
    pub(crate) fn for_ident(self, ident: ValueIdent<'_>, expected: &str) -> Self {
        match self {
            Self::Type(reason) => Self::Type(format!("{ident}: expected {expected} ({reason})")),
            other => other,
        }
    }
}

/// A WebIDL `DOMString`: WTF-16 code units, lone surrogates preserved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DOMString {
    units: Vec<u16>,
}

impl DOMString {
    /// The raw WTF-16 code units.
    #[must_use]
    pub fn as_units(&self) -> &[u16] {
        &self.units
    }

    /// Take the code units out.
    #[must_use]
    pub fn into_units(self) -> Vec<u16> {
        self.units
    }

    /// Lossy UTF-8 rendering (lone surrogates become U+FFFD).
    #[must_use]
    pub fn to_lossy_string(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }
}

impl From<&str> for DOMString {
    fn from(text: &str) -> Self {
        Self {
            units: text.encode_utf16().collect(),
        }
    }
}

impl From<String> for DOMString {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

impl<'s> FromJs<'s> for DOMString {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.to_string_units(v)
            .map(|units| Self { units })
            .map_err(|err| err.for_ident(ident, "a string"))
    }
}

/// A WebIDL `USVString`: Unicode scalar values only (lone surrogates
/// replaced with U+FFFD during conversion).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct USVString(String);

impl USVString {
    /// Borrow as UTF-8 text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Take the owned string out.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for USVString {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl From<&str> for USVString {
    fn from(text: &str) -> Self {
        Self(text.to_string())
    }
}

impl std::ops::Deref for USVString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

/// Plain `String` extraction: USVString semantics (spec `ToString`,
/// lone surrogates replaced). The ergonomic default for bodies that
/// just want owned text.
impl<'s> FromJs<'s> for String {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.to_string_spec(v)
            .map_err(|err| err.for_ident(ident, "a string"))
    }
}

impl<'s> FromJs<'s> for USVString {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.to_string_spec(v)
            .map(Self)
            .map_err(|err| err.for_ident(ident, "a string"))
    }
}

/// A WebIDL `sequence<T>`: any iterable, each element converted via
/// `T::from_js`. Arrays take a dense fast path; other iterables drive
/// the `Symbol.iterator` protocol.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sequence<T>(pub Vec<T>);

impl<T> std::ops::Deref for Sequence<T> {
    type Target = Vec<T>;
    fn deref(&self) -> &Vec<T> {
        &self.0
    }
}

impl<T> IntoIterator for Sequence<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'s, T: FromJs<'s>> FromJs<'s> for Sequence<T> {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        let handles = cx
            .iterate_to_handles(v)
            .map_err(|err| err.for_ident(ident, "an iterable"))?;
        let mut out = Vec::with_capacity(handles.len());
        for (index, element) in handles.into_iter().enumerate() {
            out.push(T::from_js(cx, element, ValueIdent::Element(index))?);
        }
        Ok(Self(out))
    }
}

/// A WebIDL `BufferSource`: an `ArrayBuffer` or any typed-array view.
/// The live byte range is copied out at conversion time; a detached
/// buffer reads as empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BufferSource(pub Vec<u8>);

impl AsRef<[u8]> for BufferSource {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl BufferSource {
    /// Take the owned bytes out.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl<'s> FromJs<'s> for BufferSource {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.buffer_source_bytes(v).map(Self).ok_or_else(|| {
            JsError::Type(format!(
                "{ident}: expected an ArrayBuffer or an ArrayBufferView"
            ))
        })
    }
}

/// A brand-checked reference to a host-data instance of `T`. The
/// referent stays parked in the ambient scope; the data is read
/// through [`HostRef::with`] (or snapshotted via [`HostRef::snapshot`])
/// because the backing object may move under a collection between
/// reads.
#[derive(Debug, Clone, Copy)]
pub struct HostRef<'s, T: 'static> {
    handle: Scoped<'s>,
    _marker: PhantomData<fn() -> T>,
}

impl<'s, T: 'static> HostRef<'s, T> {
    /// The underlying scope handle.
    #[must_use]
    pub fn handle(&self) -> Scoped<'s> {
        self.handle
    }

    /// Borrow the host data for the duration of `f`.
    pub fn with<R>(
        &self,
        cx: &MarshalCx<'_, '_, '_>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, JsError> {
        cx.with_host_data::<T, R>(self.handle, f)
    }

    /// Clone the host data out.
    pub fn snapshot(&self, cx: &MarshalCx<'_, '_, '_>) -> Result<T, JsError>
    where
        T: Clone,
    {
        self.with(cx, Clone::clone)
    }
}

impl<'s, T: 'static> FromJs<'s> for HostRef<'s, T> {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        cx.with_host_data::<T, ()>(v, |_| ())
            .map_err(|err| err.for_ident(ident, std::any::type_name::<T>()))?;
        Ok(Self {
            handle: v,
            _marker: PhantomData,
        })
    }
}

/// A callable argument. Conversion verifies callability; invocation
/// re-enters the VM through the call's execution context.
#[derive(Debug, Clone, Copy)]
pub struct Callback<'s> {
    handle: Scoped<'s>,
}

impl<'s> Callback<'s> {
    /// The underlying scope handle.
    #[must_use]
    pub fn handle(&self) -> Scoped<'s> {
        self.handle
    }

    /// Invoke with `this_value` and `args`, parking the completion
    /// value in the ambient scope.
    pub fn call(
        &self,
        cx: &mut MarshalCx<'_, '_, 's>,
        this_value: Scoped<'_>,
        args: &[Scoped<'_>],
    ) -> Result<Scoped<'s>, JsError> {
        cx.call(self.handle, this_value, args)
    }
}

impl<'s> FromJs<'s> for Callback<'s> {
    fn from_js(
        cx: &mut MarshalCx<'_, '_, 's>,
        v: Scoped<'s>,
        ident: ValueIdent<'_>,
    ) -> Result<Self, JsError> {
        if !cx.is_callable(v) {
            return Err(JsError::Type(format!("{ident}: expected a function")));
        }
        Ok(Self { handle: v })
    }
}
