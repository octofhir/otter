//! Declarative value marshalling between Rust and JavaScript.
//!
//! This is the conversion layer under Otter's declarative binding
//! surface: native code exchanges plain Rust data with the VM, and the
//! traits here own the JS side of the exchange. [`FromJs`] extracts a
//! typed Rust value out of an argument handle with WebIDL coercion
//! semantics; [`IntoJs`] builds the JS result value from a returned
//! Rust value. Both run against a [`MarshalCx`], a borrowed view over
//! (`&mut NativeCtx`, `&HandleScope`) — every intermediate JS value is
//! parked in the collector-traced handle arena, so conversions are
//! GC-sound by construction and user code never holds a raw
//! [`crate::Value`] across an allocation.
//!
//! # Contents
//! - [`MarshalCx`] — the conversion context ([`cx`]).
//! - [`FromJs`] and the extraction types ([`from_js`]):
//!   [`DOMString`], [`USVString`], [`Sequence`], [`BufferSource`],
//!   [`HostRef`], [`Callback`], [`JsValue`].
//! - [`IntoJs`] and the construction newtypes ([`into_js`]):
//!   [`ArrayBuffer`], [`Uint8Array`].
//! - [`JsError`] / [`ValueIdent`] — the binding error model ([`error`]).
//! - Interpreter scope-arena builders backing the above
//!   ([`scoped_ext`]): array-buffer / typed-array from bytes, settled
//!   promises, iterable draining.
//!
//! # Invariants
//! - No raw `Value` is held across an allocating call anywhere in this
//!   module; every JS value lives behind a [`crate::Scoped`] handle
//!   minted in the caller's scope.
//! - Spec coercions (`ToString` / `ToNumber` / `ToPrimitive`) may
//!   re-enter user JS. They require the call's
//!   [`crate::ExecutionContext`]; a context-free native call can still
//!   convert primitives, and object coercion reports a
//!   [`JsError::Type`] instead of guessing.
//! - Argument conversion is left-to-right, one value fully converted
//!   before the next starts (WebIDL). [`BufferSource`] copies its bytes
//!   at its own conversion step, so a later re-entrant coercion cannot
//!   invalidate it.
//!
//! # See also
//! - [`crate::handles`] — the handle-scope arena this layer builds on.
//! - `docs/site/src/content/docs/extensions/handle-scopes.md` — the
//!   rooting contract.
//! - `EXTENSION_API_PLAN.md` (repo root) — the design this implements.

mod cx;
mod error;
mod from_js;
mod into_js;
mod scoped_ext;

pub use cx::MarshalCx;
pub use error::{JsError, ValueIdent};
pub use from_js::{
    BufferSource, Callback, DOMString, FromJs, HostRef, JsValue, Sequence, USVString,
};
pub use into_js::{ArrayBuffer, IntoJs, Uint8Array};

#[cfg(test)]
mod tests;
