//! Intl.Segmenter implementation.
//!
//! Spec: <https://tc39.es/ecma402/#sec-intl-segmenter-constructor>
//! §18.5 Segments objects: <https://tc39.es/ecma402/#sec-segments-objects>
//! §18.6 Segment Iterator objects: <https://tc39.es/ecma402/#sec-segment-iterator-objects>

use icu_segmenter::{GraphemeClusterSegmenter, SentenceSegmenter, WordSegmenter};

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::value::RegisterValue;

use super::options_utils::get_option_string;
use super::payload::{
    self, IntlPayload, SegmentIteratorData, SegmenterData, SegmenterGranularity, SegmentsData,
};

// ═══════════════════════════════════════════════════════════════════
//  Class descriptor
// ═══════════════════════════════════════════════════════════════════

pub fn segmenter_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Segmenter")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Segmenter",
            0,
            segmenter_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("segment", 1, segmenter_segment),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("resolvedOptions", 0, segmenter_resolved_options),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "supportedLocalesOf",
                1,
                segmenter_supported_locales_of,
            ),
        ))
}

/// Native function descriptors for Segments prototype methods.
/// Installed in `intl/mod.rs` during intrinsic initialization.
pub fn segments_prototype_methods() -> Vec<NativeFunctionDescriptor> {
    vec![
        NativeFunctionDescriptor::method("containing", 1, segments_containing),
    ]
}

/// Native function descriptors for SegmentIterator prototype methods.
pub fn segment_iterator_prototype_methods() -> Vec<NativeFunctionDescriptor> {
    vec![NativeFunctionDescriptor::method(
        "next",
        0,
        segment_iterator_next,
    )]
}

// ═══════════════════════════════════════════════════════════════════
//  §18.1.1 Intl.Segmenter(locales, options)
// ═══════════════════════════════════════════════════════════════════

fn segmenter_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let options_arg = args.get(1).copied().unwrap_or_else(RegisterValue::undefined);

    let locale = super::resolve_locale(locales_arg, runtime)?;

    let granularity = parse_enum(
        get_option_string(options_arg, "granularity", runtime)?,
        SegmenterGranularity::from_str_opt,
        SegmenterGranularity::Grapheme,
        "granularity",
        runtime,
    )?;

    let data = SegmenterData {
        locale,
        granularity,
    };

    let prototype = runtime.intrinsics().intl_segmenter_prototype();
    let handle = payload::construct_intl(IntlPayload::Segmenter(data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §18.3.3 Intl.Segmenter.prototype.segment(string)
// ═══════════════════════════════════════════════════════════════════

/// Returns a Segments object (iterable) per §18.5.
fn segmenter_segment(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_segmenter_data(this, runtime)?.clone();
    let str_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let input = runtime
        .js_to_string(str_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("Segmenter.segment: {e}").into()))?;

    let segments_data = SegmentsData {
        input: input.to_string(),
        granularity: data.granularity,
        locale: data.locale,
    };

    let prototype = runtime.intrinsics().intl_segments_prototype();
    let handle =
        payload::construct_intl(IntlPayload::Segments(segments_data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §18.5.2.1 %Segments%.prototype.containing(index)
// ═══════════════════════════════════════════════════════════════════

/// Returns a segment data object for the segment containing the code unit
/// at the given index, or `undefined` if the index is out of range.
fn segments_containing(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_segments_data(this, runtime)?.clone();

    let index_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let index = index_arg
        .as_number()
        .or_else(|| index_arg.as_i32().map(f64::from))
        .unwrap_or(0.0) as i64;

    if index < 0 || index as usize >= data.input.len() {
        return Ok(RegisterValue::undefined());
    }
    let index = index as usize;

    let breakpoints = compute_breakpoints(&data.input, data.granularity);

    // Find the segment that contains `index`.
    for (start, end, is_word_like) in &breakpoints {
        if index >= *start && index < *end {
            let segment = &data.input[*start..*end];
            return build_segment_data_object(segment, *start, &data.input, *is_word_like, runtime);
        }
    }

    Ok(RegisterValue::undefined())
}

// ═══════════════════════════════════════════════════════════════════
//  §18.5.2.2 %Segments%.prototype[@@iterator]()
// ═══════════════════════════════════════════════════════════════════

/// Creates a Segment Iterator from a Segments object.
/// Installed as `[Symbol.iterator]` on %Segments%.prototype in intl/mod.rs.
pub fn segments_symbol_iterator(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_segments_data(this, runtime)?.clone();

    let breakpoints = compute_breakpoints(&data.input, data.granularity);

    let iter_data = SegmentIteratorData {
        input: data.input,
        granularity: data.granularity,
        locale: data.locale,
        breakpoints,
        position: 0,
    };

    let prototype = runtime.intrinsics().intl_segment_iterator_prototype();
    let handle =
        payload::construct_intl(IntlPayload::SegmentIterator(iter_data), prototype, runtime);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §18.6.2.1 %SegmentIterator%.prototype.next()
// ═══════════════════════════════════════════════════════════════════

fn segment_iterator_next(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(crate::object::ObjectHandle)
        .ok_or_else(|| {
            VmNativeCallError::Internal("SegmentIterator.next: expected object".into())
        })?;

    // Read current state and advance position atomically.
    let (position, input, breakpoints_len) = {
        let data: &mut IntlPayload = runtime.native_payload_mut::<IntlPayload>(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("SegmentIterator: {e}").into()))?;
        let iter = data.as_segment_iterator_mut().ok_or_else(|| {
            VmNativeCallError::Internal(
                "called on incompatible Intl receiver (not SegmentIterator)".into(),
            )
        })?;
        let pos = iter.position;
        let len = iter.breakpoints.len();
        let input_clone = iter.input.clone();
        if pos < len {
            iter.position += 1;
        }
        (pos, input_clone, len)
    };

    if position >= breakpoints_len {
        // Done — return { value: undefined, done: true }
        let result = runtime.alloc_object();
        let prop_value = runtime.intern_property_name("value");
        let _ = runtime.objects_mut().set_property(
            result,
            prop_value,
            RegisterValue::undefined(),
        );
        let prop_done = runtime.intern_property_name("done");
        let _ = runtime.objects_mut().set_property(
            result,
            prop_done,
            RegisterValue::from_bool(true),
        );
        return Ok(RegisterValue::from_object_handle(result.0));
    }

    // Re-read the breakpoint (position was already advanced).
    let (start, end, is_word_like) = {
        let data: &IntlPayload = runtime.native_payload::<IntlPayload>(handle)
            .map_err(|e| VmNativeCallError::Internal(format!("SegmentIterator: {e}").into()))?;
        let iter = data.as_segment_iterator().ok_or_else(|| {
            VmNativeCallError::Internal("not SegmentIterator".into())
        })?;
        iter.breakpoints[position]
    };

    let segment = input[start..end].to_string();

    // Build the segment data object.
    let seg_obj =
        build_segment_data_object(&segment, start, &input, is_word_like, runtime)?;

    // Return { value: segObj, done: false }
    let result = runtime.alloc_object();
    let prop_value = runtime.intern_property_name("value");
    let _ = runtime
        .objects_mut()
        .set_property(result, prop_value, seg_obj);
    let prop_done = runtime.intern_property_name("done");
    let _ = runtime.objects_mut().set_property(
        result,
        prop_done,
        RegisterValue::from_bool(false),
    );
    Ok(RegisterValue::from_object_handle(result.0))
}

// ═══════════════════════════════════════════════════════════════════
//  §18.3.4 Intl.Segmenter.prototype.resolvedOptions()
// ═══════════════════════════════════════════════════════════════════

fn segmenter_resolved_options(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let data = require_segmenter_data(this, runtime)?.clone();
    let obj = runtime.alloc_object();

    let prop_locale = runtime.intern_property_name("locale");
    let s = runtime.alloc_string(data.locale.as_str());
    let _ = runtime.objects_mut().set_property(obj, prop_locale, RegisterValue::from_object_handle(s.0));

    let prop_gran = runtime.intern_property_name("granularity");
    let s = runtime.alloc_string(data.granularity.as_str());
    let _ = runtime.objects_mut().set_property(obj, prop_gran, RegisterValue::from_object_handle(s.0));

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  Intl.Segmenter.supportedLocalesOf(locales)
// ═══════════════════════════════════════════════════════════════════

fn segmenter_supported_locales_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let locales_arg = args.first().copied().unwrap_or_else(RegisterValue::undefined);
    let locale_list = super::canonicalize_locale_list_from_value(locales_arg, runtime)?;
    let arr = runtime.alloc_array();
    for locale in &locale_list {
        let s = runtime.alloc_string(locale.as_str());
        runtime
            .objects_mut()
            .push_element(arr, RegisterValue::from_object_handle(s.0))
            .map_err(|e| VmNativeCallError::Internal(format!("supportedLocalesOf: {e:?}").into()))?;
    }
    Ok(RegisterValue::from_object_handle(arr.0))
}

// ═══════════════════════════════════════════════════════════════════
//  ICU4X segmentation + breakpoint computation
// ═══════════════════════════════════════════════════════════════════

/// Compute segment breakpoints: Vec of (byte_start, byte_end, is_word_like).
fn compute_breakpoints(
    input: &str,
    granularity: SegmenterGranularity,
) -> Vec<(usize, usize, Option<bool>)> {
    match granularity {
        SegmenterGranularity::Grapheme => compute_grapheme_breakpoints(input),
        SegmenterGranularity::Word => compute_word_breakpoints(input),
        SegmenterGranularity::Sentence => compute_sentence_breakpoints(input),
    }
}

fn compute_grapheme_breakpoints(input: &str) -> Vec<(usize, usize, Option<bool>)> {
    let segmenter = GraphemeClusterSegmenter::new();
    let breaks: Vec<usize> = segmenter.segment_str(input).collect();
    let mut result = Vec::new();
    let mut start = 0;
    for end in breaks {
        if end > start {
            result.push((start, end, None));
        }
        start = end;
    }
    result
}

fn compute_word_breakpoints(input: &str) -> Vec<(usize, usize, Option<bool>)> {
    let segmenter = WordSegmenter::new_auto(Default::default());
    let iter = segmenter.segment_str(input);
    let mut result = Vec::new();
    let mut prev = 0;
    for bp in iter {
        if bp > prev {
            let segment = &input[prev..bp];
            let is_word_like = segment.chars().any(|c| c.is_alphanumeric());
            result.push((prev, bp, Some(is_word_like)));
        }
        prev = bp;
    }
    result
}

fn compute_sentence_breakpoints(input: &str) -> Vec<(usize, usize, Option<bool>)> {
    let segmenter = SentenceSegmenter::new(Default::default());
    let breaks: Vec<usize> = segmenter.segment_str(input).collect();
    let mut result = Vec::new();
    let mut start = 0;
    for end in breaks {
        if end > start {
            result.push((start, end, None));
        }
        start = end;
    }
    result
}

/// Build a segment data object: `{ segment, index, input, isWordLike? }`.
fn build_segment_data_object(
    segment: &str,
    index: usize,
    input: &str,
    is_word_like: Option<bool>,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let obj = runtime.alloc_object();

    let prop_segment = runtime.intern_property_name("segment");
    let s = runtime.alloc_string(segment);
    let _ = runtime.objects_mut().set_property(
        obj,
        prop_segment,
        RegisterValue::from_object_handle(s.0),
    );

    let prop_index = runtime.intern_property_name("index");
    let _ = runtime.objects_mut().set_property(
        obj,
        prop_index,
        RegisterValue::from_i32(index as i32),
    );

    let prop_input = runtime.intern_property_name("input");
    let s_input = runtime.alloc_string(input);
    let _ = runtime.objects_mut().set_property(
        obj,
        prop_input,
        RegisterValue::from_object_handle(s_input.0),
    );

    if let Some(wl) = is_word_like {
        let prop_wl = runtime.intern_property_name("isWordLike");
        let _ = runtime
            .objects_mut()
            .set_property(obj, prop_wl, RegisterValue::from_bool(wl));
    }

    Ok(RegisterValue::from_object_handle(obj.0))
}

// ═══════════════════════════════════════════════════════════════════
//  Enum implementations
// ═══════════════════════════════════════════════════════════════════

impl SegmenterGranularity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Grapheme => "grapheme",
            Self::Word => "word",
            Self::Sentence => "sentence",
        }
    }
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "grapheme" => Some(Self::Grapheme),
            "word" => Some(Self::Word),
            "sentence" => Some(Self::Sentence),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════════════════════

fn require_segmenter_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a SegmenterData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("Segmenter: {e}").into())
    })?;
    payload.as_segmenter().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not Segmenter)".into(),
        )
    })
}

fn require_segments_data<'a>(
    this: &RegisterValue,
    runtime: &'a crate::interpreter::RuntimeState,
) -> Result<&'a SegmentsData, VmNativeCallError> {
    let payload = payload::require_intl_payload(this, runtime).map_err(|e| {
        VmNativeCallError::Internal(format!("Segments: {e}").into())
    })?;
    payload.as_segments().ok_or_else(|| {
        VmNativeCallError::Internal(
            "called on incompatible Intl receiver (not Segments)".into(),
        )
    })
}

fn parse_enum<T>(
    value: Option<String>,
    from_str: fn(&str) -> Option<T>,
    default: T,
    name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<T, VmNativeCallError> {
    match value {
        None => Ok(default),
        Some(s) => from_str(&s).ok_or_else(|| range_error(runtime, &format!("Invalid {name} option"))),
    }
}

fn range_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_range_error(message) {
        Ok(err) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(err.0)),
        Err(e) => VmNativeCallError::Internal(format!("RangeError alloc: {e}").into()),
    }
}
