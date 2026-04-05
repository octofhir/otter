//! Integration tests for ECMA-402 Intl.Segmenter.
//!
//! §18 Segmenter: <https://tc39.es/ecma402/#segmenter-objects>
//! §18.5 Segments: <https://tc39.es/ecma402/#sec-segments-objects>
//! §18.6 Segment Iterator: <https://tc39.es/ecma402/#sec-segment-iterator-objects>

use otter_vm::source::compile_eval;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str) -> RegisterValue {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value()
}

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

fn run_string(source: &str) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value();
    let h = v
        .as_object_handle()
        .map(otter_vm::object::ObjectHandle)
        .expect("expected string object");
    runtime
        .objects()
        .string_value(h)
        .ok()
        .flatten()
        .expect("expected string")
        .to_string()
}

fn run_f64(source: &str) -> f64 {
    let v = run(source);
    v.as_number()
        .unwrap_or_else(|| panic!("expected number, got {v:?}"))
}

// ── Constructor ──────────────────────────────────────────────────

#[test]
fn segmenter_is_function() {
    assert!(run_bool("typeof Intl.Segmenter === 'function'"));
}

#[test]
fn segmenter_constructor_no_args() {
    assert!(run_bool("typeof new Intl.Segmenter() === 'object'"));
}

#[test]
fn segmenter_constructor_with_locale() {
    assert!(run_bool("typeof new Intl.Segmenter('en') === 'object'"));
}

// ── segment() returns Segments object ────────────────────────────

#[test]
fn segment_returns_object() {
    assert!(run_bool(
        "typeof new Intl.Segmenter('en').segment('abc') === 'object'"
    ));
}

#[test]
fn segment_is_iterable() {
    // Segments object must have [Symbol.iterator]
    assert!(run_bool(
        "var s = new Intl.Segmenter('en').segment('abc'); \
         typeof s[Symbol.iterator] === 'function'"
    ));
}

#[test]
fn segment_spread_grapheme_count() {
    // Spreading into array should give one segment per grapheme cluster.
    let n = run_f64(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc')]; \
         arr.length",
    );
    assert_eq!(n, 3.0);
}

#[test]
fn segment_spread_has_segment_property() {
    let s = run_string(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc')]; \
         arr[0].segment",
    );
    assert_eq!(s, "a");
}

#[test]
fn segment_spread_has_index_property() {
    let n = run_f64(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc')]; \
         arr[1].index",
    );
    assert_eq!(n, 1.0);
}

#[test]
fn segment_spread_has_input_property() {
    let s = run_string(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc')]; \
         arr[0].input",
    );
    assert_eq!(s, "abc");
}

#[test]
fn segment_word_spread_has_is_word_like() {
    assert!(run_bool(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'word' }).segment('hello world')]; \
         arr[0].isWordLike === true"
    ));
}

#[test]
fn segment_word_spread_count() {
    // "hello world" should produce at least 3 segments: "hello", " ", "world"
    let n = run_f64(
        "var arr = [...new Intl.Segmenter('en', { granularity: 'word' }).segment('hello world')]; \
         arr.length",
    );
    assert!(n >= 3.0, "expected >= 3 word segments, got: {n}");
}

// ── containing() ────────────────────────────────────────────────

#[test]
fn containing_returns_segment_at_index() {
    let s = run_string(
        "var segs = new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc'); \
         segs.containing(0).segment",
    );
    assert_eq!(s, "a");
}

#[test]
fn containing_second_char() {
    let s = run_string(
        "var segs = new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc'); \
         segs.containing(1).segment",
    );
    assert_eq!(s, "b");
}

#[test]
fn containing_out_of_range_returns_undefined() {
    assert!(run_bool(
        "var segs = new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc'); \
         segs.containing(100) === undefined"
    ));
}

#[test]
fn containing_negative_returns_undefined() {
    assert!(run_bool(
        "var segs = new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('abc'); \
         segs.containing(-1) === undefined"
    ));
}

// ── for-of iteration ─────────────────────────────────────────────

#[test]
fn for_of_collects_segments() {
    let n = run_f64(
        "var result = []; \
         for (var seg of new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('hi')) { \
             result.push(seg.segment); \
         } \
         result.length",
    );
    assert_eq!(n, 2.0);
}

#[test]
fn for_of_first_segment_value() {
    let s = run_string(
        "var result = []; \
         for (var seg of new Intl.Segmenter('en', { granularity: 'grapheme' }).segment('hi')) { \
             result.push(seg.segment); \
         } \
         result[0]",
    );
    assert_eq!(s, "h");
}

// ── Segment Iterator toStringTag ─────────────────────────────────

#[test]
fn segment_iterator_to_string_tag() {
    let s = run_string(
        "var segs = new Intl.Segmenter('en').segment('x'); \
         var iter = segs[Symbol.iterator](); \
         Object.prototype.toString.call(iter)",
    );
    assert_eq!(s, "[object Segmenter String Iterator]");
}

// ── resolvedOptions() ────────────────────────────────────────────

#[test]
fn resolved_options_locale() {
    let s = run_string("new Intl.Segmenter('en').resolvedOptions().locale");
    assert!(
        s.starts_with("en"),
        "expected locale starting with 'en', got: {s}"
    );
}

#[test]
fn resolved_options_granularity_default() {
    let s = run_string("new Intl.Segmenter('en').resolvedOptions().granularity");
    assert_eq!(s, "grapheme");
}

#[test]
fn resolved_options_granularity_word() {
    let s = run_string(
        "new Intl.Segmenter('en', { granularity: 'word' }).resolvedOptions().granularity",
    );
    assert_eq!(s, "word");
}

#[test]
fn resolved_options_granularity_sentence() {
    let s = run_string(
        "new Intl.Segmenter('en', { granularity: 'sentence' }).resolvedOptions().granularity",
    );
    assert_eq!(s, "sentence");
}

// ── supportedLocalesOf() ─────────────────────────────────────────

#[test]
fn supported_locales_of_returns_array() {
    assert!(run_bool(
        "Array.isArray(Intl.Segmenter.supportedLocalesOf('en'))"
    ));
}
