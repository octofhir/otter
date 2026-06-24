//! `Intl.*` constructor + namespace dispatch.
//!
//! The runtime exposes `Op::NewIntl dst, kind_const, locale_reg,
//! options_reg` and routes through [`construct`]. Compiled bytecode
//! never carries individual ICU-specific knobs — every option is
//! resolved here at construction time and stashed on the
//! [`crate::intl::payload::IntlPayload`] for later method calls.
//!
//! # Contents
//! - [`construct`] — entry point for `new Intl.<Class>(locale,
//!   options?)`.
//! - [`IntlError`] — failure mode the dispatcher converts to
//!   `VmError`.

use crate::Value;
use crate::intl::collator;
use crate::intl::date_time_format;
use crate::intl::display_names;
use crate::intl::number_format;
use crate::intl::payload::{IntlKind, IntlPayload, JsIntl};
use crate::intl::plural_rules;
use crate::intl::relative_time_format;
use crate::intl::segmenter;

/// Failure modes for `Intl.*` construction / method calls.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum IntlError {
    /// Class name is not a known `Intl.*` constructor.
    #[error("Intl.{0} is not defined")]
    UnknownClass(String),
    /// Method name is not registered on the receiver's prototype.
    #[error("Intl.{class}.prototype.{method} is not defined")]
    UnknownMember {
        /// JS-visible class name.
        class: &'static str,
        /// JS-visible method name.
        method: String,
    },
    /// Argument was the wrong type or shape.
    #[error("Intl.{class}.{method}: argument {index} {reason}")]
    BadArgument {
        /// JS-visible class name.
        class: &'static str,
        /// JS-visible method name.
        method: &'static str,
        /// Argument index.
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
    /// Invalid option value — surfaces as a JS `RangeError` (ECMA-402
    /// reports out-of-range option values this way).
    #[error("{message}")]
    Range {
        /// Error message.
        message: String,
    },
    /// Pass-through for ICU engine errors.
    #[error("Intl.{class}.{method}: {message}")]
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

impl From<otter_gc::OutOfMemory> for IntlError {
    fn from(err: otter_gc::OutOfMemory) -> Self {
        Self::OutOfMemory {
            requested_bytes: err.requested_bytes(),
            heap_limit_bytes: err.heap_limit_bytes(),
        }
    }
}

/// Dispatch `new Intl.<class>(locale?, options?)`.
pub fn construct(
    class: &str,
    locale: &Value,
    options: &Value,
    gc_heap: &mut otter_gc::GcHeap,
) -> Result<Value, IntlError> {
    let kind = IntlKind::from_class_name(class)
        .ok_or_else(|| IntlError::UnknownClass(class.to_string()))?;
    let payload = match kind {
        IntlKind::Collator => IntlPayload::Collator(collator::resolve(locale, options, gc_heap)),
        IntlKind::NumberFormat => {
            IntlPayload::NumberFormat(number_format::resolve(locale, options, gc_heap)?)
        }
        IntlKind::DateTimeFormat => {
            IntlPayload::DateTimeFormat(date_time_format::resolve(locale, options, gc_heap)?)
        }
        IntlKind::PluralRules => {
            IntlPayload::PluralRules(plural_rules::resolve(locale, options, gc_heap))
        }
        IntlKind::RelativeTimeFormat => {
            IntlPayload::RelativeTimeFormat(relative_time_format::resolve(locale, options, gc_heap))
        }
        // `Intl.ListFormat` is constructed through its own `NativeCtx`
        // option ladder (firing getters in spec order) and never reaches
        // this heap-only dispatcher.
        IntlKind::ListFormat => return Err(IntlError::UnknownClass("ListFormat".to_string())),
        IntlKind::DisplayNames => {
            IntlPayload::DisplayNames(display_names::resolve(locale, options, gc_heap))
        }
        IntlKind::Segmenter => IntlPayload::Segmenter(segmenter::resolve(locale, options, gc_heap)),
        // `Intl.Locale` / `Intl.DurationFormat` are constructed through
        // their own `NativeCtx` path (they must fire option getters in
        // spec order) and never reach this heap-only dispatcher.
        IntlKind::Locale => return Err(IntlError::UnknownClass("Locale".to_string())),
        IntlKind::DurationFormat => {
            return Err(IntlError::UnknownClass("DurationFormat".to_string()));
        }
    };
    Ok(Value::intl(JsIntl::new(gc_heap, payload).map_err(
        |_| IntlError::OutOfMemory {
            requested_bytes: 0,
            heap_limit_bytes: 0,
        },
    )?))
}
