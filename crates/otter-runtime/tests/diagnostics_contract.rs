//! Public diagnostic contract coverage for compile-time failures.
//!
//! These tests exercise the runtime boundary shape consumed by CLI renderers
//! and embedders: stable code, source URL, byte range, and help text.

use otter_runtime::{OtterError, Runtime};

#[test]
fn check_file_syntax_error_includes_source_url_range_code_and_help() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("entry.ts");
    std::fs::write(&path, "const = ;\n").expect("write source");

    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime.check_file(&path).expect_err("syntax error");
    let diagnostic = first_compile_diagnostic(&err);

    assert_eq!(diagnostic.code, "SYNTAX_ERROR");
    assert_eq!(
        diagnostic.source_url.as_deref(),
        Some(path.to_str().unwrap())
    );
    assert_eq!(diagnostic.span, diagnostic.range);
    assert!(diagnostic.range.is_some());
    assert!(
        diagnostic
            .help
            .as_deref()
            .is_some_and(|help| !help.is_empty())
    );
}

#[test]
fn check_file_typescript_unsupported_includes_source_url_range_code_and_help() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("entry.ts");
    std::fs::write(&path, "enum E { A }\n").expect("write source");

    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime.check_file(&path).expect_err("unsupported enum");
    let diagnostic = first_compile_diagnostic(&err);

    assert_eq!(diagnostic.code, "TS_UNSUPPORTED");
    assert_eq!(
        diagnostic.source_url.as_deref(),
        Some(path.to_str().unwrap())
    );
    assert_eq!(diagnostic.span, diagnostic.range);
    assert!(diagnostic.range.is_some());
    assert!(
        diagnostic
            .help
            .as_deref()
            .is_some_and(|help| !help.is_empty())
    );
}

fn first_compile_diagnostic(err: &OtterError) -> &otter_runtime::Diagnostic {
    match err {
        OtterError::Compile { diagnostics } => diagnostics.first().expect("diagnostic"),
        other => panic!("expected compile error, got {other:?}"),
    }
}
