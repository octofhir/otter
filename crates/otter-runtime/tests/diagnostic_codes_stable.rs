//! Snapshot guard for the closed [`otter_runtime::DiagnosticCode`]
//! set per ENGINE_REFACTOR_EXECUTION_PLAN §P2.3.
//!
//! Asserts the wire-format code strings produced across the
//! workspace (otter-syntax, otter-pm-manifest, otter-vm JSON
//! errors, otter-runtime mappers) all live in the closed enum.
//! Adding a code in any of those producer sites without extending
//! [`otter_runtime::DiagnosticCode`] is therefore a compile-clean
//! test failure rather than silent string drift.

use otter_runtime::{DiagnosticCategory, DiagnosticCode};

#[test]
fn every_variant_is_unique_and_round_trips() {
    use std::collections::HashSet;
    let mut seen: HashSet<&'static str> = HashSet::new();
    for code in DiagnosticCode::all() {
        let text = code.as_str();
        assert!(seen.insert(text), "duplicate code text {text:?}");
        let parsed = DiagnosticCode::parse(text)
            .unwrap_or_else(|| panic!("from_str rejected canonical text {text:?}"));
        assert_eq!(parsed, *code);
    }
}

#[test]
fn category_buckets_match_p2_3_plan() {
    use DiagnosticCategory as Cat;
    use DiagnosticCode::*;
    for (code, expected) in [
        (SyntaxError, Cat::Parse),
        (TsUnsupported, Cat::Parse),
        (FeatureNotInSlice, Cat::Parse),
        (InvalidRegexp, Cat::Parse),
        (ModuleResolutionError, Cat::Resolve),
        (ModuleGraphCycle, Cat::Resolve),
        (CapabilityDenied, Cat::Permission),
        (ModuleCapabilityDenied, Cat::Permission),
        (CompileUnknown, Cat::Compile),
        (TypeMismatch, Cat::Runtime),
        (Uncaught, Cat::Runtime),
        (JsonCyclic, Cat::Runtime),
        (PmManifestEmptyName, Cat::PackageManager),
        (PmManifestEmptyVersion, Cat::PackageManager),
        (PmManifestEmptyDependencyName, Cat::PackageManager),
        (PmManifestEmptyDependencyRange, Cat::PackageManager),
        (VmBytecodeInvariant, Cat::Internal),
        (RuntimeShutdown, Cat::Internal),
        (DumpJson, Cat::Internal),
    ] {
        assert_eq!(code.category(), expected, "{code:?}");
    }
}

#[test]
fn syntax_diagnostic_code_string_lives_in_the_enum() {
    // `otter_syntax::SyntaxDiagnostic::from_message` stamps the
    // canonical wire-format `SYNTAX_ERROR` string. Layer ordering
    // (otter-syntax is below otter-runtime) keeps the literal in
    // place, so we contract-check the string here.
    let diag = otter_syntax::SyntaxDiagnostic::from_message("x");
    let typed = DiagnosticCode::parse(&diag.code)
        .expect("otter-syntax stamped a code outside the closed set");
    assert_eq!(typed, DiagnosticCode::SyntaxError);
}

#[test]
fn pm_manifest_diagnostic_codes_live_in_the_enum() {
    use otter_pm_manifest::PackageManifest;

    // Each empty-field shape produces one of the closed PM codes
    // through `PackageManifest::validate`.
    let cases: &[(&str, DiagnosticCode)] = &[
        (
            r#"{ "name": "", "version": "1.0.0" }"#,
            DiagnosticCode::PmManifestEmptyName,
        ),
        (
            r#"{ "name": "x", "version": "" }"#,
            DiagnosticCode::PmManifestEmptyVersion,
        ),
        (
            r#"{ "name": "x", "version": "1.0.0", "dependencies": { "": "^1" } }"#,
            DiagnosticCode::PmManifestEmptyDependencyName,
        ),
        (
            r#"{ "name": "x", "version": "1.0.0", "dependencies": { "y": "" } }"#,
            DiagnosticCode::PmManifestEmptyDependencyRange,
        ),
    ];

    for (source, expected) in cases {
        let manifest = PackageManifest::parse_json(source).expect("parse manifest");
        let diagnostics = manifest.validate();
        let code_text = &diagnostics
            .iter()
            .find(|d| d.code == expected.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "expected diagnostic with code {} for {source:?}; got {diagnostics:?}",
                    expected.as_str()
                )
            })
            .code;
        let typed = DiagnosticCode::parse(code_text)
            .unwrap_or_else(|| panic!("otter-pm-manifest stamped unknown code {code_text:?}"));
        assert_eq!(&typed, expected, "{source}");
    }
}

#[test]
fn vm_json_diagnostic_codes_live_in_the_enum() {
    use otter_runtime::{OtterError, Runtime, SourceInput};

    let mut runtime = Runtime::builder().build().expect("runtime");
    let err = runtime
        .run_script(
            SourceInput::from_javascript("JSON.parse('{')"),
            "<json-parse-test>",
        )
        .expect_err("JSON.parse should reject malformed input");
    let diagnostic = match err {
        OtterError::Runtime { diagnostic } => *diagnostic,
        other => panic!("expected runtime error, got {other:?}"),
    };
    let typed = DiagnosticCode::parse(&diagnostic.code)
        .unwrap_or_else(|| panic!("otter-vm stamped unknown JSON code {:?}", diagnostic.code));
    assert_eq!(typed, DiagnosticCode::Uncaught);
}
