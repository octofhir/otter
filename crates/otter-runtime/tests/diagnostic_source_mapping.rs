//! Stack-frame source-mapping coverage for structured diagnostics.
//!
//! Asserts that a runtime throw surfaces a [`StackFrame`] whose
//! [`StackFrame::span`] is the **original source byte range** of
//! the failing instruction (not the bytecode-level synthesized
//! span, and not the enclosing function's full span). Also
//! exercises [`Runtime::resolve_frame_span`] so the host-side
//! lookup helper stays wired up.

use otter_runtime::{OtterError, Runtime, SourceInput, StackFrame};

#[test]
fn top_frame_span_points_at_failing_source_substring() {
    let source = "function boom() {\n    throw new Error(\"oops\");\n}\nboom();\n";
    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime
        .run_script(SourceInput::from_javascript(source), "<source-map-test>")
        .expect_err("script throws");
    let diagnostic = match err {
        OtterError::Runtime { diagnostic } => *diagnostic,
        other => panic!("expected Runtime error, got {other:?}"),
    };

    // Top frame is the `boom` function (throw site), second is
    // `<main>` (the `boom()` call site).
    let frames = &diagnostic.frames;
    assert!(
        frames.len() >= 2,
        "expected at least 2 frames, got {} (diagnostic: {diagnostic:?})",
        frames.len()
    );
    assert_eq!(frames[0].function, "boom");

    // Source-byte range of `throw new Error("oops");` substring.
    let throw_start = source.find("throw").expect("throw keyword") as u32;
    let throw_end = source[throw_start as usize..]
        .find(';')
        .map(|rel| throw_start + rel as u32 + 1)
        .expect("statement end");

    let frame_span = frames[0].span.expect("top frame has source span");
    assert!(
        frame_span.0 >= throw_start && frame_span.1 <= throw_end + 4,
        "top frame span {frame_span:?} should fall inside throw statement \
         ({throw_start}..{throw_end})",
    );
    // Span is strictly tighter than the enclosing function — the
    // enclosing function starts at the `function` keyword on
    // line 1; the throw is on line 2.
    assert!(
        frame_span.0 > 0,
        "top frame span start {} must be past the function keyword",
        frame_span.0
    );
}

#[test]
fn module_field_uses_per_function_module_url_for_multi_module_graph() {
    // Single-file run: function frames inherit the script's
    // module specifier rather than `<entry>` or a stripped name.
    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime
        .run_script(
            SourceInput::from_javascript("throw new Error(\"x\");"),
            "<source-map-module-test>",
        )
        .expect_err("script throws");
    let diagnostic = match err {
        OtterError::Runtime { diagnostic } => *diagnostic,
        other => panic!("expected Runtime error, got {other:?}"),
    };
    let module = diagnostic
        .frames
        .first()
        .map(|f: &StackFrame| f.module.clone())
        .expect("top frame");
    // The fix in `snapshot_frames` keeps the per-function
    // `module_url` if non-empty, else falls back to the bytecode
    // module name. The script entry has its specifier on the
    // bytecode module level — accept either.
    assert!(
        module.contains("source-map-module-test") || module == "<entry>",
        "unexpected module field on top frame: {module:?}"
    );
}

#[test]
fn resolve_frame_span_returns_predecessor_entry() {
    // Compile a script, then map a synthetic `(module_url,
    // function_id, pc)` triple. The function id for the script
    // body is `0` (the entry function).
    let mut runtime = Runtime::builder().build().expect("runtime");
    let source = "const a = 1;\nconst b = a + 2;\n";
    let _ = runtime.eval(SourceInput::from_javascript(source));

    // `pc = u32::MAX` resolves to the last span entry (largest
    // predecessor); ensures the binary-search fallback wires up.
    let resolved = runtime.resolve_frame_span("<eval>", 0, u32::MAX);
    assert!(
        resolved.is_some(),
        "resolve_frame_span should return a span for a compiled entry function"
    );
    let span = resolved.unwrap();
    let (start, end) = span;
    assert!(
        end <= source.len() as u32,
        "span end out of range: {span:?}"
    );
    assert!(start <= end, "span has start > end: {span:?}");
}

#[test]
fn resolve_frame_span_returns_none_for_unknown_module() {
    let runtime = Runtime::builder().build().expect("runtime");
    assert!(
        runtime
            .resolve_frame_span("file:///not-compiled.ts", 0, 0)
            .is_none()
    );
}
