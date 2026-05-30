//! `Intl.Segmenter` — locale-aware text segmentation.
//!
//! Foundation surface: grapheme segmentation by Unicode scalar
//! values (one segment per code point), word segmentation by ASCII
//! whitespace, sentence segmentation by ASCII `.` / `!` / `?`. ICU
//! CLDR break-iterator integration is filed for the wider Intl
//! follow-up.
//!
//! # See also
//! - <https://tc39.es/ecma402/#segmenter-objects>

use otter_gc::raw::RawGc;

use crate::intl::helpers::{coerce_locale, options_object, read_string_option};
use crate::intl::payload::{IntlPayload, SegmenterPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, Value};

/// Resolve constructor options for this Intl class.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> SegmenterPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    SegmenterPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        granularity: read_string_option(opts_ref, "granularity", "grapheme", gc_heap),
    }
}

fn require_payload(
    ctx: &NativeCtx<'_>,
    name: &'static str,
) -> Result<SegmenterPayload, NativeError> {
    let bad = || NativeError::TypeError {
        name,
        reason: "intrinsic called on a non-Intl.Segmenter receiver".to_string(),
    };
    let intl = ctx.this_value().as_intl(ctx.heap()).ok_or_else(bad)?;
    match intl.payload_clone(ctx.heap()) {
        IntlPayload::Segmenter(p) => Ok(p),
        _ => Err(bad()),
    }
}

/// Segment `text` per the granularity. Returns a vector of
/// `(byte_start, segment)` pairs.
fn segment(text: &str, granularity: &str) -> Vec<(usize, String)> {
    match granularity {
        "word" => {
            let mut out: Vec<(usize, String)> = Vec::new();
            let mut start: Option<usize> = None;
            let bytes = text.as_bytes();
            for (i, c) in text.char_indices() {
                if c.is_whitespace() {
                    if let Some(s) = start.take() {
                        out.push((s, text[s..i].to_string()));
                    }
                    out.push((i, c.to_string()));
                } else if start.is_none() {
                    start = Some(i);
                }
                let _ = bytes;
            }
            if let Some(s) = start {
                out.push((s, text[s..].to_string()));
            }
            out
        }
        "sentence" => {
            let mut out: Vec<(usize, String)> = Vec::new();
            let mut start = 0usize;
            for (i, c) in text.char_indices() {
                if c == '.' || c == '!' || c == '?' {
                    let end = i + c.len_utf8();
                    out.push((start, text[start..end].to_string()));
                    start = end;
                }
            }
            if start < text.len() {
                out.push((start, text[start..].to_string()));
            }
            out
        }
        _ => {
            // grapheme — code-point granularity foundation.
            text.char_indices()
                .map(|(i, c)| (i, c.to_string()))
                .collect()
        }
    }
}

/// §18.4.3 `Intl.Segmenter.prototype.segment(string)` — returns an
/// array of segment-data objects. Spec returns a live `Segments`
/// iterator; foundation returns an array (each element is a
/// `{segment, index, input, isWordLike?}` plain object) which is
/// iterable through the existing iterator-protocol path.
pub(crate) fn segmenter_segment(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "segment")?;
    let first = args.first();
    let text = if let Some(s) = first.and_then(|v| v.as_string(ctx.heap())) {
        s.to_lossy_string(ctx.heap())
    } else if let Some(n) = first.and_then(|v| v.as_number()) {
        n.to_display_string()
    } else if let Some(b) = first.and_then(|v| v.as_boolean()) {
        (if b { "true" } else { "false" }).to_string()
    } else {
        return Err(NativeError::TypeError {
            name: "segment",
            reason: "argument 0 must be a string".to_string(),
        });
    };
    let segments = segment(&text, &payload.granularity);
    let input_value = Value::string(JsString::from_str(&text, ctx.heap_mut())?);
    let granularity_word = payload.granularity == "word";
    let mut prepared: Vec<(Value, i32, bool)> = Vec::with_capacity(segments.len());
    for (idx, seg) in segments {
        let seg_str = Value::string(JsString::from_str(&seg, ctx.heap_mut())?);
        let wordlike = granularity_word && seg.chars().any(char::is_alphanumeric);
        prepared.push((seg_str, idx as i32, wordlike));
    }
    let prepared_values: Vec<Value> = prepared.iter().map(|(value, _, _)| *value).collect();
    let mut elements: Vec<Value> = Vec::with_capacity(prepared.len());
    for (seg_str, idx, wordlike) in &prepared {
        let obj =
            ctx.alloc_object_with_roots(&[seg_str, &input_value], &[&prepared_values, &elements])?;
        let heap = ctx.heap_mut();
        crate::object::set(obj, heap, "segment", *seg_str);
        crate::object::set(obj, heap, "index", Value::number_i32(*idx));
        crate::object::set(obj, heap, "input", input_value);
        if granularity_word {
            crate::object::set(obj, heap, "isWordLike", Value::boolean(*wordlike));
        }
        elements.push(Value::object(obj));
    }
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    let element_roots = elements.clone();
    let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for &slot in &roots {
            visitor(slot);
        }
        this_value.trace_value_slots(visitor);
        input_value.trace_value_slots(visitor);
        for v in &prepared_values {
            v.trace_value_slots(visitor);
        }
        for v in &element_roots {
            v.trace_value_slots(visitor);
        }
    };
    let arr =
        crate::array::from_elements_with_roots(ctx.heap_mut(), elements, &mut external_visit)?;
    Ok(Value::array(arr))
}

/// §18.3.4 `Intl.Segmenter.prototype.resolvedOptions()`.
pub(crate) fn segmenter_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let granularity = Value::string(JsString::from_str(&payload.granularity, ctx.heap_mut())?);
    let obj = ctx.alloc_object_with_roots(&[&locale, &granularity], &[])?;
    let heap = ctx.heap_mut();
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "granularity", granularity);
    Ok(Value::object(obj))
}
