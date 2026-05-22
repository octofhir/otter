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

use std::sync::LazyLock;

use crate::Value;
use crate::intl::dispatch::IntlError;
use crate::intl::helpers::{coerce_locale, js_string, options_object, read_string_option};
use crate::intl::payload::{IntlPayload, SegmenterPayload};
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};

/// Resolve constructor options for this Intl class.
pub fn resolve(locale: &Value, options: &Value, gc_heap: &otter_gc::GcHeap) -> SegmenterPayload {
    let opts = options_object(Some(options));
    let opts_ref = opts.as_ref();
    SegmenterPayload {
        locale: coerce_locale(Some(locale), gc_heap),
        granularity: read_string_option(opts_ref, "granularity", "grapheme", gc_heap),
    }
}

fn require_payload(args: &IntrinsicArgs<'_>) -> Result<SegmenterPayload, IntrinsicError> {
    match args.receiver {
        Value::Intl(intl) => match intl.payload_clone(args.gc_heap) {
            IntlPayload::Segmenter(p) => Ok(p),
            _ => Err(IntrinsicError::BadReceiver {
                expected: "Intl.Segmenter",
            }),
        },
        _ => Err(IntrinsicError::BadReceiver {
            expected: "Intl.Segmenter",
        }),
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

/// §18.5.3 `segment(string)` — returns an array of segment-data
/// objects. Spec returns a live `Segments` iterator; foundation
/// returns an array (each element is a `{segment, index, input,
/// isWordLike?}` plain object) which is iterable through the
/// existing iterator-protocol path.
fn impl_segment(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let text = match args.args.first() {
        Some(Value::String(s)) => s.to_lossy_string(args.gc_heap),
        Some(Value::Number(n)) => n.to_display_string(),
        Some(Value::Boolean(b)) => (if *b { "true" } else { "false" }).to_string(),
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a string",
            });
        }
    };
    let segments = segment(&text, &payload.granularity);
    let input_value = Value::String(crate::string::JsString::from_str(&text, args.gc_heap)?);
    let granularity_word = payload.granularity == "word";
    // Pre-allocate JsString values; once gc_heap is borrowed we can
    // only call object:: APIs.
    let mut prepared: Vec<(Value, i32, bool)> = Vec::with_capacity(segments.len());
    for (idx, seg) in segments {
        let seg_str = Value::String(crate::string::JsString::from_str(&seg, args.gc_heap)?);
        let wordlike = granularity_word && seg.chars().any(char::is_alphanumeric);
        prepared.push((seg_str, idx as i32, wordlike));
    }
    let prepared_values: Vec<Value> = prepared.iter().map(|(value, _, _)| *value).collect();
    let mut elements: Vec<Value> = Vec::with_capacity(prepared.len());
    for (seg_str, idx, wordlike) in &prepared {
        let obj =
            args.alloc_object_rooted(&[seg_str, &input_value], &[&prepared_values, &elements])?;
        let heap = &mut *args.gc_heap;
        crate::object::set(obj, heap, "segment", *seg_str);
        crate::object::set(
            obj,
            heap,
            "index",
            Value::Number(crate::number::NumberValue::from_i32(*idx)),
        );
        crate::object::set(obj, heap, "input", input_value);
        if granularity_word {
            crate::object::set(obj, heap, "isWordLike", Value::Boolean(*wordlike));
        }
        elements.push(Value::Object(obj));
    }
    Ok(Value::Array(args.array_from_elements_rooted(
        elements,
        &[&input_value],
        &[&prepared_values],
    )?))
}

fn impl_resolved_options(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let payload = require_payload(args)?;
    let locale = js_string(&payload.locale, args.gc_heap).map_err(intl_to_intrinsic)?;
    let granularity = js_string(&payload.granularity, args.gc_heap).map_err(intl_to_intrinsic)?;
    let obj = args.alloc_object_rooted(&[&locale, &granularity], &[])?;
    let heap = &mut *args.gc_heap;
    crate::object::set(obj, heap, "locale", locale);
    crate::object::set(obj, heap, "granularity", granularity);
    Ok(Value::Object(obj))
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
            expected: "Intl.Segmenter",
        },
    }
}

/// `Intl.Segmenter.prototype` table.
pub static SEGMENTER_PROTOTYPE_TABLE: LazyLock<IntrinsicTable> = LazyLock::new(|| {
    crate::intrinsics!(
        Intl,
        "segment"          / 1 => impl_segment,
        "resolvedOptions"  / 0 => impl_resolved_options,
    )
});

#[must_use]
/// Convenience accessor used by [`super::lookup_prototype`].
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    SEGMENTER_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Intl, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intl::payload::JsIntl;
    use crate::string::JsString;

    #[test]
    fn segment_uses_intrinsic_rooted_young_allocation() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let receiver = Value::Intl(
            JsIntl::new(
                &mut gc_heap,
                IntlPayload::Segmenter(SegmenterPayload {
                    locale: "en-US".to_string(),
                    granularity: "word".to_string(),
                }),
            )
            .expect("intl"),
        );
        let input = Value::String(JsString::from_str("alpha beta", &mut gc_heap).expect("input"));
        let args = [input];
        let before = gc_heap.stats().new_allocated_bytes;

        let result = impl_segment(&mut IntrinsicArgs {
            receiver: &receiver,
            args: &args,
            gc_heap: &mut gc_heap,
            allocation_roots: &[],
        })
        .expect("segment");

        let after = gc_heap.stats().new_allocated_bytes;
        assert!(
            after > before,
            "Intl.Segmenter.prototype.segment should allocate segment objects and result array in young space"
        );
        assert!(matches!(result, Value::Array(_)));
    }
}
