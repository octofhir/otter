//! `Intl.ListFormat` — locale-aware list joining.
//!
//! Foundation surface: English templates for the three spec types:
//! - conjunction: `"a, b, and c"`
//! - disjunction: `"a, b, or c"`
//! - unit:        `"a, b, c"`
//!
//! # See also
//! - <https://tc39.es/ecma402/#listformat-objects>

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, js_string, options_object, read_string_option};
use crate::intl::payload::{IntlPayload, ListFormatPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// Resolve constructor options for this Intl class.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> ListFormatPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    ListFormatPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        kind: read_string_option(opts_ref, "type", "conjunction", gc_heap),
        style: read_string_option(opts_ref, "style", "long", gc_heap),
    }
}

fn require_payload(args: &IntrinsicArgs<'_>) -> Result<ListFormatPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload_clone(args.gc_heap) {
            IntlPayload::ListFormat(p) => Ok(p),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.ListFormat",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.ListFormat",
        }),
    }
}

fn join(items: &[String], payload: &ListFormatPayload) -> String {
    let conjunction = match payload.kind.as_str() {
        "disjunction" => "or",
        "unit" => "",
        _ => "and",
    };
    let narrow = payload.style == "narrow";
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        2 => {
            if conjunction.is_empty() || narrow {
                format!("{}, {}", items[0], items[1])
            } else {
                format!("{} {} {}", items[0], conjunction, items[1])
            }
        }
        n => {
            let head = items[..n - 1].join(", ");
            if conjunction.is_empty() {
                format!("{}, {}", head, items[n - 1])
            } else {
                format!("{}, {} {}", head, conjunction, items[n - 1])
            }
        }
    }
}

fn collect_items(
    value: Option<&Value>,
    gc_heap: &otter_gc::GcHeap,
) -> Result<Vec<String>, IntrinsicError> {
    match value {
        Some(Value::Array(arr)) => {
            let values = crate::array::with_elements(*arr, gc_heap, |elements| elements.to_vec());
            let mut out: Vec<String> = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    Value::String(s) => out.push(s.to_lossy_string(gc_heap)),
                    Value::Number(n) => out.push(n.to_display_string()),
                    Value::Boolean(b) => out.push((if b { "true" } else { "false" }).to_string()),
                    _ => {
                        return Err(IntrinsicError::BadArgument {
                            index: 0,
                            reason: "list elements must be strings",
                        });
                    }
                }
            }
            Ok(out)
        }
        _ => Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "argument must be an Array",
        }),
    }
}

/// §13.5.3 `format(list)`.
fn impl_format(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let heap = &*args.gc_heap;
    let items = collect_items(args.args.first(), heap)?;
    let rendered = join(&items, &payload);
    Ok(Value::string(crate::string::JsString::from_str(
        &rendered,
        args.gc_heap,
    )?))
}

/// §13.5.4 `formatToParts(list)` — single-literal-part fallback.
fn impl_format_to_parts(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = impl_format(args)?;
    let literal = Value::string(crate::string::JsString::from_str("literal", args.gc_heap)?);
    let part = args.alloc_object_rooted(&[&literal, &s], &[])?;
    {
        let heap = &mut *args.gc_heap;
        crate::object::set(part, heap, "type", literal);
        crate::object::set(part, heap, "value", s);
    }
    Ok(Value::array(args.array_from_elements_rooted(
        [Value::object(part)],
        &[],
        &[],
    )?))
}

fn impl_resolved_options(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let locale = js_string(&payload.locale, args.gc_heap).map_err(intl_to_intrinsic)?;
    let kind = js_string(&payload.kind, args.gc_heap).map_err(intl_to_intrinsic)?;
    let style = js_string(&payload.style, args.gc_heap).map_err(intl_to_intrinsic)?;
    let obj = args.alloc_object_rooted(&[&locale, &kind, &style], &[])?;
    let heap = &mut *args.gc_heap;
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "type", kind);
    crate::object::set(obj, heap, "style", style);
    Ok(Value::object(obj))
}

fn intl_to_intrinsic(err: IntlError) -> IntrinsicError {
    match err {
        IntlError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } => IntrinsicError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        },
        _ => IntrinsicError::BadReceiver {
            expected: "Intl.ListFormat",
        },
    }
}

/// `Intl.ListFormat.prototype` table.
pub static LIST_FORMAT_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "format"           / 1 => impl_format,
        "formatToParts"    / 1 => impl_format_to_parts,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

#[must_use]
/// Convenience accessor used by [`super::lookup_prototype`].
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    LIST_FORMAT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}
