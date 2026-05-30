//! `Intl.ListFormat` — locale-aware list joining.
//!
//! Foundation surface: English templates for the three spec types:
//! - conjunction: `"a, b, and c"`
//! - disjunction: `"a, b, or c"`
//! - unit:        `"a, b, c"`
//!
//! # See also
//! - <https://tc39.es/ecma402/#listformat-objects>

use otter_gc::raw::RawGc;

use crate::intl::helpers::{coerce_locale, options_object, read_string_option};
use crate::intl::payload::{IntlPayload, ListFormatPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

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

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<ListFormatPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.ListFormat receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::ListFormat(p) => Ok(p),
        _ => Err(bad()),
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
) -> Result<Vec<String>, NativeError> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Err(NativeError::TypeError {
            name: "format",
            reason: "argument 0 must be an Array".to_string(),
        });
    };
    let values = crate::array::with_elements(arr, gc_heap, |elements| elements.to_vec());
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        if let Some(s) = v.as_string(gc_heap) {
            out.push(s.to_lossy_string(gc_heap));
        } else if let Some(n) = v.as_number() {
            out.push(n.to_display_string());
        } else if let Some(b) = v.as_boolean() {
            out.push((if b { "true" } else { "false" }).to_string());
        } else {
            return Err(NativeError::TypeError {
                name: "format",
                reason: "argument 0 list elements must be strings".to_string(),
            });
        }
    }
    Ok(out)
}

/// §13.5.3 `Intl.ListFormat.prototype.format(list)`.
pub(crate) fn list_format_format(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "format")?;
    let items = collect_items(args.first(), ctx.heap())?;
    let rendered = join(&items, &payload);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

/// §13.5.4 `Intl.ListFormat.prototype.formatToParts(list)` —
/// single-literal-part fallback.
pub(crate) fn list_format_format_to_parts(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let s = list_format_format(ctx, args)?;
    let literal = Value::string(JsString::from_str("literal", ctx.heap_mut())?);
    let part = ctx.alloc_object_with_roots(&[&literal, &s], &[])?;
    crate::object::set(part, ctx.heap_mut(), "type", literal);
    crate::object::set(part, ctx.heap_mut(), "value", s);
    let elements = vec![Value::object(part)];
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let element_roots = elements.clone();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for &slot in &roots {
            visitor(slot);
        }
        this_value.trace_value_slots(visitor);
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr =
        crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut external_visit)?;
    Ok(Value::array(arr))
}

/// §13.5.5 `Intl.ListFormat.prototype.resolvedOptions()`.
pub(crate) fn list_format_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let kind = Value::string(JsString::from_str(&payload.kind, ctx.heap_mut())?);
    let style = Value::string(JsString::from_str(&payload.style, ctx.heap_mut())?);
    let obj = ctx.alloc_object_with_roots(&[&locale, &kind, &style], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "type", kind);
    crate::object::set(obj, heap, "style", style);
    Ok(Value::object(obj))
}
