//! `Intl.Segmenter` — locale-aware text segmentation.
//!
//! Foundation surface: UAX #29 grapheme, word, and sentence
//! boundaries over ECMAScript WTF-16 input. ICU CLDR break-iterator
//! locale tailoring is filed for the wider Intl follow-up.
//!
//! # See also
//! - <https://tc39.es/ecma402/#segmenter-objects>

use otter_gc::raw::RawGc;
use unicode_segmentation::UnicodeSegmentation;

use crate::intl::helpers::{DEFAULT_LOCALE, get_string_option, require_options_object};
use crate::intl::payload::{IntlPayload, SegmenterPayload};
use crate::string::JsString;
use crate::{NativeCtx, NativeError, NativeFunction, Value};

const CLASS: &str = "Segmenter";

/// §19.1.1 InitializeSegmenter — fires `localeMatcher` / `granularity`
/// getters in spec order with ToString coercion + RangeError validation;
/// canonicalizes the locale.
pub fn resolve_ctx(
    ctx: &mut NativeCtx<'_>,
    locales: Value,
    options: Value,
) -> Result<SegmenterPayload, NativeError> {
    let requested = crate::intl::supported::canonicalize_locale_list(ctx, locales)?;
    let locale = requested
        .into_iter()
        .find(|tag| crate::intl::supported::is_supported(tag))
        .unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let options = require_options_object(options, CLASS)?;
    let _matcher = get_string_option(
        ctx,
        options,
        "localeMatcher",
        CLASS,
        &["lookup", "best fit"],
        None,
    )?;
    let granularity = get_string_option(
        ctx,
        options,
        "granularity",
        CLASS,
        &["grapheme", "word", "sentence"],
        Some("grapheme"),
    )?
    .unwrap_or_else(|| "grapheme".to_string());
    Ok(SegmenterPayload {
        locale,
        granularity,
    })
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

#[derive(Debug)]
struct SegmentPart {
    index: usize,
    units: Vec<u16>,
    is_word_like: bool,
}

fn code_point_at(units: &[u16], index: usize) -> (u32, usize) {
    let unit = units[index];
    if (0xD800..=0xDBFF).contains(&unit)
        && let Some(&low) = units.get(index + 1)
        && (0xDC00..=0xDFFF).contains(&low)
    {
        let high_ten = (unit as u32) - 0xD800;
        let low_ten = (low as u32) - 0xDC00;
        return (0x10000 + ((high_ten << 10) | low_ten), 2);
    }
    (unit as u32, 1)
}

#[derive(Debug, Clone, Copy)]
enum BoundaryKind {
    Grapheme,
    Word,
    Sentence,
}

/// Segment `text` per the granularity. Indices and slices are based on
/// ECMAScript's WTF-16 code units so lone surrogates round-trip.
fn segment(text: &[u16], granularity: &str) -> Vec<SegmentPart> {
    let kind = match granularity {
        "word" => BoundaryKind::Word,
        "sentence" => BoundaryKind::Sentence,
        _ => BoundaryKind::Grapheme,
    };
    segment_valid_runs(text, kind)
}

fn segment_valid_runs(text: &[u16], kind: BoundaryKind) -> Vec<SegmentPart> {
    let mut out = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut i = 0usize;
    while i < text.len() {
        let (code_point, len) = code_point_at(text, i);
        if char::from_u32(code_point).is_some() {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else {
            if let Some(start) = run_start.take() {
                push_valid_run_segments(&mut out, start, &text[start..i], kind);
            }
            out.push(SegmentPart {
                index: i,
                units: text[i..i + len].to_vec(),
                is_word_like: false,
            });
        }
        i += len;
    }
    if let Some(start) = run_start {
        push_valid_run_segments(&mut out, start, &text[start..], kind);
    }
    out
}

fn push_valid_run_segments(
    out: &mut Vec<SegmentPart>,
    run_start: usize,
    units: &[u16],
    kind: BoundaryKind,
) {
    let Ok(text) = String::from_utf16(units) else {
        return;
    };
    match kind {
        BoundaryKind::Grapheme => {
            for (byte_index, segment) in text.grapheme_indices(true) {
                push_utf8_segment(out, run_start, &text, byte_index, segment, false);
            }
        }
        BoundaryKind::Word => {
            for (byte_index, segment) in text.split_word_bound_indices() {
                let is_word_like = segment.chars().any(char::is_alphanumeric);
                push_utf8_segment(out, run_start, &text, byte_index, segment, is_word_like);
            }
        }
        BoundaryKind::Sentence => {
            for (byte_index, segment) in text.split_sentence_bound_indices() {
                push_utf8_segment(out, run_start, &text, byte_index, segment, false);
            }
        }
    }
}

fn push_utf8_segment(
    out: &mut Vec<SegmentPart>,
    run_start: usize,
    full_text: &str,
    byte_index: usize,
    segment: &str,
    is_word_like: bool,
) {
    let utf16_index = full_text[..byte_index].encode_utf16().count();
    out.push(SegmentPart {
        index: run_start + utf16_index,
        units: segment.encode_utf16().collect(),
        is_word_like,
    });
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
    let text_arg = args.first().copied().unwrap_or_else(Value::undefined);
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "segment",
            reason: "missing execution context for string coercion".to_string(),
        })?;
    let input_string = crate::coerce::to_js_string_or_throw(ctx.cx.interp, &exec, &text_arg)
        .map_err(|err| crate::native_function::vm_to_native_error(ctx.cx.interp, err, "segment"))?;
    let input_units = input_string.to_utf16_vec(ctx.heap());
    let segments = segment(&input_units, &payload.granularity);
    let input_value = Value::string(input_string);
    let granularity_word = payload.granularity == "word";
    let mut prepared: Vec<(Value, i32, bool)> = Vec::with_capacity(segments.len());
    let roots = ctx.collect_native_roots();
    let this_value = *ctx.this_value();
    for segment in segments {
        let mut segment_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
            this_value.trace_value_slots(visitor);
            input_value.trace_value_slots(visitor);
            for (value, _, _) in &prepared {
                value.trace_value_slots(visitor);
            }
        };
        let seg_str = Value::string(JsString::from_utf16_units_with_roots(
            &segment.units,
            ctx.heap_mut(),
            &mut segment_visit,
        )?);
        prepared.push((seg_str, segment.index as i32, segment.is_word_like));
    }
    let prepared_values: Vec<Value> = prepared.iter().map(|(value, _, _)| *value).collect();
    let mut elements: Vec<Value> = Vec::with_capacity(prepared.len());
    for (seg_str, idx, wordlike) in &prepared {
        let mut obj =
            ctx.alloc_object_with_roots(&[seg_str, &input_value], &[&prepared_values, &elements])?;
        if let Some(proto) = ctx.cx.interp.object_prototype_object_opt() {
            crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
        }
        let heap = ctx.heap_mut();
        crate::object::set(&mut obj, heap, "segment", *seg_str);
        crate::object::set(&mut obj, heap, "index", Value::number_i32(*idx));
        crate::object::set(&mut obj, heap, "input", input_value);
        if granularity_word {
            crate::object::set(&mut obj, heap, "isWordLike", Value::boolean(*wordlike));
        }
        elements.push(Value::object(obj));
    }
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
    let arr_value = Value::array(arr);
    let mut method_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        for &slot in &roots {
            visitor(slot);
        }
        this_value.trace_value_slots(visitor);
        input_value.trace_value_slots(visitor);
        arr_value.trace_value_slots(visitor);
    };
    let containing = Value::native_function(NativeFunction::new_static_with_roots(
        ctx.heap_mut(),
        "containing",
        1,
        segments_containing,
        &mut method_visit,
    )?);
    crate::array::set_named_property(arr, ctx.heap_mut(), "containing", containing)?;
    let iterator_fn = Value::native_function(NativeFunction::new_static_with_roots(
        ctx.heap_mut(),
        "[Symbol.iterator]",
        0,
        segments_symbol_iterator,
        &mut method_visit,
    )?);
    let iterator_sym = ctx
        .cx
        .interp
        .well_known_symbols()
        .get(crate::WellKnown::Iterator);
    crate::array::set_symbol_property(arr, ctx.heap_mut(), iterator_sym, iterator_fn);
    Ok(Value::array(arr))
}

fn segments_symbol_iterator(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    let (interp, context) = ctx.interp_mut_and_context();
    let Some(context) = context.as_ref() else {
        return Err(NativeError::TypeError {
            name: "[Symbol.iterator]",
            reason: "missing execution context for iterator creation".to_string(),
        });
    };
    interp
        .array_iterator_method(context, receiver, "values", &[])
        .map_err(|err| crate::native_function::vm_to_native_error(interp, err, "[Symbol.iterator]"))
}

fn segments_containing(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(arr) = ctx.this_value().as_array() else {
        return Err(NativeError::TypeError {
            name: "containing",
            reason: "intrinsic called on a non-Segments receiver".to_string(),
        });
    };
    let index_arg = args.first().copied().unwrap_or_else(Value::undefined);
    let number = {
        let (interp, context) = ctx.interp_mut_and_context();
        let Some(context) = context.as_ref() else {
            return Err(NativeError::TypeError {
                name: "containing",
                reason: "missing execution context for index coercion".to_string(),
            });
        };
        crate::coerce::to_number_or_throw(interp, context, &index_arg)
            .map_err(|err| NativeError::TypeError {
                name: "containing",
                reason: err.to_string(),
            })?
            .as_f64()
    };
    let n = if number.is_nan() || number == 0.0 {
        0.0
    } else {
        let magnitude = number.abs().floor();
        if magnitude == 0.0 {
            0.0
        } else {
            number.signum() * magnitude
        }
    };
    let len = crate::array::len(arr, ctx.heap());
    if len == 0 || n.is_sign_negative() {
        return Ok(Value::undefined());
    }
    let input_len = segment_record_input_len(crate::array::get(arr, ctx.heap(), 0), ctx);
    if !n.is_finite() || n >= input_len as f64 {
        return Ok(Value::undefined());
    }
    let n = n as usize;
    for i in 0..len {
        let record = crate::array::get(arr, ctx.heap(), i);
        let Some(start) = segment_record_index(record, ctx) else {
            continue;
        };
        let next = if i + 1 < len {
            segment_record_index(crate::array::get(arr, ctx.heap(), i + 1), ctx)
                .unwrap_or(input_len)
        } else {
            input_len
        };
        if n >= start && n < next {
            return Ok(record);
        }
    }
    Ok(Value::undefined())
}

fn segment_record_index(record: Value, ctx: &NativeCtx<'_>) -> Option<usize> {
    let obj = record.as_object()?;
    let index = crate::object::get(obj, ctx.heap(), "index")?;
    let number = index.as_number()?.as_f64();
    if number.is_finite() && number >= 0.0 {
        Some(number as usize)
    } else {
        None
    }
}

fn segment_record_input_len(record: Value, ctx: &NativeCtx<'_>) -> usize {
    let Some(obj) = record.as_object() else {
        return 0;
    };
    crate::object::get(obj, ctx.heap(), "input")
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|string| string.len() as usize)
        .unwrap_or(0)
}

/// §18.3.4 `Intl.Segmenter.prototype.resolvedOptions()`.
pub(crate) fn segmenter_resolved_options(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
) -> Result<Value, NativeError> {
    let payload = require_payload(ctx, "resolvedOptions")?;
    let locale = Value::string(JsString::from_str(&payload.locale, ctx.heap_mut())?);
    let granularity = Value::string(JsString::from_str(&payload.granularity, ctx.heap_mut())?);
    let mut obj = ctx.alloc_object_with_roots(&[&locale, &granularity], &[])?;
    if let Some(proto) = ctx.cx.interp.object_prototype_object_opt() {
        crate::object::set_prototype(obj, ctx.heap_mut(), Some(proto));
    }
    let heap = ctx.heap_mut();
    crate::object::set(&mut obj, heap, "locale", locale);
    crate::object::set(&mut obj, heap, "granularity", granularity);
    Ok(Value::object(obj))
}
