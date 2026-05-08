//! `Temporal.<Class>.<static>(args...)` and `Temporal.<Class>` /
//! `Temporal.Now.<view>` dispatch backend.
//!
//! Mirrors the [`crate::math`] / [`crate::symbol_dispatch`]
//! pattern: the runtime exposes two opcodes
//! ([`Op::TemporalCall`](otter_bytecode::Op::TemporalCall) and
//! [`Op::TemporalLoad`](otter_bytecode::Op::TemporalLoad)) that
//! bottom out here.
//!
//! # Contents
//! - [`call`] — entry point for `Temporal.<Class>.<method>(args...)`
//!   (factories / comparators) and `Temporal.Now.<view>(...)` calls.
//! - [`load_static`] — read accessors against `Temporal.<member>`
//!   when the result is a value rather than a callable result. The
//!   foundation has no static-only members today (every member is
//!   reached through a call), so this returns
//!   [`TemporalError::UnknownMember`] for now and is reserved for
//!   future calendars / unit constants.
//! - [`TemporalError`] — failure mode the dispatcher converts to
//!   `VmError`.
//!
//! # See also
//! - <https://tc39.es/proposal-temporal/>

use crate::Value;
use crate::temporal::now;
use crate::temporal::{duration, instant, plain_date, plain_date_time, plain_time};

/// Failure modes returned by [`call`] / [`load_static`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum TemporalError {
    /// `Temporal.<Class>.<method>` is not a registered member.
    #[error("Temporal.{class}.{method} is not defined")]
    UnknownMember {
        /// JS-visible class name.
        class: String,
        /// JS-visible method name.
        method: String,
    },
    /// Argument was the wrong type or value for the called member.
    #[error("Temporal.{class}.{method}: argument {index} {reason}")]
    BadArgument {
        /// JS-visible class name.
        class: &'static str,
        /// JS-visible method name.
        method: &'static str,
        /// Argument index (0-based).
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
    /// Pass-through for `temporal_rs` engine errors. The engine
    /// reports a structured error; the foundation surfaces the
    /// stringified form so user code can match on it.
    #[error("Temporal.{class}.{method}: {message}")]
    Engine {
        /// JS-visible class name.
        class: &'static str,
        /// JS-visible method name.
        method: &'static str,
        /// Error message.
        message: String,
    },
    /// String allocation failed.
    #[error("out of memory: requested {requested_bytes} bytes, heap limit {heap_limit_bytes}")]
    OutOfMemory {
        /// Bytes requested.
        requested_bytes: u64,
        /// Heap cap (`0` = unlimited).
        heap_limit_bytes: u64,
    },
}

impl From<crate::string::StringError> for TemporalError {
    fn from(err: crate::string::StringError) -> Self {
        match err {
            crate::string::StringError::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            } => Self::OutOfMemory {
                requested_bytes,
                heap_limit_bytes,
            },
        }
    }
}

/// Dispatch `Temporal.<class>.<method>(args...)` via the typed
/// [`TemporalClassId`] / [`TemporalMethod`] operands.
///
/// # See also
/// - <https://tc39.es/proposal-temporal/#sec-temporal-instant-objects>
pub fn call(
    string_heap: &crate::string::StringHeap,
    gc_heap: &otter_gc::GcHeap,
    class: otter_bytecode::method_id::TemporalClassId,
    method: otter_bytecode::method_id::TemporalMethod,
    args: &[Value],
) -> Result<Value, TemporalError> {
    use otter_bytecode::method_id::TemporalClassId as C;
    if matches!(class, C::Now) {
        return now::dispatch(string_heap, method, args);
    }
    match class {
        C::Now => unreachable!("handled above"),
        C::Instant => instant::dispatch_static(string_heap, method, args),
        C::Duration => duration::dispatch_static(string_heap, gc_heap, method, args),
        C::PlainDate => plain_date::dispatch_static(string_heap, method, args),
        C::PlainTime => plain_time::dispatch_static(string_heap, method, args),
        C::PlainDateTime => plain_date_time::dispatch_static(string_heap, method, args),
    }
}

/// `Temporal.<member>` static read. Reserved for future calendar /
/// unit constants — today every member is reached through [`call`],
/// so unknown names raise [`TemporalError::UnknownMember`].
pub fn load_static(name: &str) -> Result<Value, TemporalError> {
    Err(TemporalError::UnknownMember {
        class: "Temporal".to_string(),
        method: name.to_string(),
    })
}
