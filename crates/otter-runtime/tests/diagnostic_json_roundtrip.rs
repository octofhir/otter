//! `--json` round-trip parity for every structured diagnostic category.
//!
//! Acceptance: "Sample failures across all seven categories
//! produce both pretty and `--json` outputs that round-trip
//! through `serde`."
//!
//! We exercise each [`otter_runtime::DiagnosticCategory`] bucket
//! with at least one canonical sample diagnostic. The test:
//!
//! 1. builds an [`otter_runtime::OtterError`] / [`otter_runtime::Diagnostic`]
//!    whose `code` lives in the closed [`otter_runtime::DiagnosticCode`] set;
//! 2. serializes it through the stable wire format
//!    ([`otter_runtime::OtterError::to_json`] for top-level errors and
//!    `serde_json::to_string_pretty` for standalone diagnostics);
//! 3. decodes the produced JSON back through `serde_json::from_str` into a
//!    generic [`serde_json::Value`] (the DTOs carry admitted source handles,
//!    so they are serialize-only);
//! 4. asserts the decoded document equals the DTO's serde model and
//!    re-serializes byte-identically against the original JSON.
//!
//! Byte-identical re-serialization pins the wire shape: any field
//! whose text does not decode to the DTO's own model (or any rename)
//! immediately fails.

use otter_runtime::{Diagnostic, DiagnosticCategory, DiagnosticCode, DiagnosticKind, OtterError};

/// One sample diagnostic per plan-mandated category. The
/// `Internal` bucket is also covered so the round-trip surface is
/// exhaustive over [`DiagnosticCategory`].
fn category_samples() -> Vec<(DiagnosticCategory, Diagnostic)> {
    vec![
        (
            DiagnosticCategory::Parse,
            Diagnostic::ts_unsupported("enum is not supported", (0, 12))
                .with_source_url("file:///fixture.ts"),
        ),
        (
            DiagnosticCategory::Resolve,
            Diagnostic::syntax("cannot resolve `./missing.ts`")
                .with_code_enum(DiagnosticCode::ModuleResolutionError)
                .with_source_url("file:///fixture.ts")
                .with_help("check the import specifier"),
        ),
        (
            DiagnosticCategory::Permission,
            Diagnostic::permission(
                "import of `https://example.com/x.ts` requires capability `net`",
            )
            .with_code_enum(DiagnosticCode::ModuleCapabilityDenied),
        ),
        (
            DiagnosticCategory::Compile,
            Diagnostic::new(
                DiagnosticKind::Internal,
                DiagnosticCode::CompileUnknown,
                "unknown compiler error variant",
            ),
        ),
        (
            DiagnosticCategory::Runtime,
            Diagnostic::new(
                DiagnosticKind::Type,
                DiagnosticCode::Uncaught,
                "uncaught exception: Error: boom",
            )
            .with_source_url("file:///fixture.ts")
            .with_range((10, 30)),
        ),
        (
            DiagnosticCategory::PackageManager,
            Diagnostic::new(
                DiagnosticKind::Internal,
                DiagnosticCode::PmManifestEmptyName,
                "package name must not be empty when present",
            ),
        ),
        (
            DiagnosticCategory::Internal,
            Diagnostic::new(
                DiagnosticKind::Internal,
                DiagnosticCode::VmBytecodeInvariant,
                "bytecode invariant violation",
            ),
        ),
        (
            DiagnosticCategory::Load,
            // The `Load` bucket has no codes routed yet (loader
            // collapses load+resolve into MODULE_RESOLUTION_ERROR
            // by design). We still round-trip a free-form
            // diagnostic stamped with `ModuleResolutionError` so
            // the bucket is exercised end-to-end.
            Diagnostic::syntax("file not found: ./missing.ts")
                .with_code_enum(DiagnosticCode::ModuleResolutionError)
                .with_source_url("file:///fixture.ts"),
        ),
    ]
}

#[test]
fn every_category_diagnostic_round_trips_byte_identical() {
    for (category, diagnostic) in category_samples() {
        // Sanity: code must live in the closed set.
        let code = DiagnosticCode::parse(&diagnostic.code).unwrap_or_else(|| {
            panic!(
                "[{category:?}] diagnostic code {:?} is not in the closed set",
                diagnostic.code
            )
        });
        // Sanity: category derived from the code matches the
        // intent of the sample (except for the `Load` bucket
        // where we deliberately reuse `Resolve`).
        if category != DiagnosticCategory::Load {
            assert_eq!(
                code.category(),
                category,
                "[{category:?}] code {:?} maps to category {:?}",
                code.as_str(),
                code.category()
            );
        }

        let first = serde_json::to_string_pretty(&diagnostic).expect("serialize diagnostic");
        let parsed: serde_json::Value =
            serde_json::from_str(&first).expect("deserialize diagnostic");
        assert_eq!(
            parsed,
            serde_json::to_value(&diagnostic).expect("diagnostic model"),
            "[{category:?}] decoded diagnostic diverged from its model"
        );
        assert_eq!(parsed["code"], diagnostic.code.as_str());
        assert_eq!(parsed["message"], diagnostic.message.as_str());
        let second = serde_json::to_string_pretty(&parsed).expect("re-serialize diagnostic");
        assert_eq!(
            first, second,
            "[{category:?}] diagnostic JSON round-trip diverged"
        );
    }
}

#[test]
fn otter_error_envelope_round_trips_byte_identical() {
    // Compile-side error envelope: vec of diagnostics, one per
    // category that surfaces as `Compile`.
    let compile_err = OtterError::Compile {
        diagnostics: category_samples()
            .into_iter()
            .map(|(_, diagnostic)| diagnostic)
            .collect(),
    };
    assert_round_trip(&compile_err);

    // Runtime-side envelope: single diagnostic with frames.
    let runtime_err = OtterError::Runtime {
        diagnostic: Box::new(Diagnostic::new(
            DiagnosticKind::Type,
            DiagnosticCode::Uncaught,
            "uncaught exception: TypeError: boom",
        )),
    };
    assert_round_trip(&runtime_err);

    // Capability envelope.
    let capability_err = OtterError::Capability {
        capability: "net".to_string(),
        detail: Some("denied by --deny-net".to_string()),
    };
    assert_round_trip(&capability_err);

    // Timeout envelope.
    assert_round_trip(&OtterError::Timeout { elapsed_ms: 4242 });

    // OOM envelope.
    assert_round_trip(&OtterError::OutOfMemory {
        requested_bytes: 1 << 20,
        heap_limit_bytes: 1 << 19,
    });

    // Internal envelope (bug-class — `Internal` category).
    assert_round_trip(&OtterError::Internal {
        code: DiagnosticCode::VmBytecodeInvariant.as_str().to_string(),
        message: "missing return".to_string(),
    });
}

fn assert_round_trip(err: &OtterError) {
    let first = err.to_json_pretty().expect("serialize error");
    // The envelope is the exact wire shape `--json` writes to stdout:
    // `{"error": <OtterError>}` plus a trailing newline.
    let parsed: serde_json::Value =
        serde_json::from_str(&first).expect("deserialize error envelope");
    assert_eq!(
        parsed["error"],
        serde_json::to_value(err).expect("error model"),
        "decoded OtterError diverged from its model"
    );
    assert!(parsed["error"]["kind"].is_string(), "envelope lost its tag");
    let mut second = serde_json::to_string_pretty(&parsed).expect("re-serialize error");
    second.push('\n');
    assert_eq!(first, second, "OtterError JSON round-trip diverged");
}

#[test]
fn diagnostic_cause_chain_round_trips() {
    // §20.5.6.1.1 InstallErrorCause: the `cause` chain is part of
    // the wire shape. Build a 2-deep chain and assert structural
    // round-trip identity.
    let inner = Diagnostic::new(
        DiagnosticKind::Type,
        DiagnosticCode::TypeError,
        "inner failure",
    );
    let outer = Diagnostic::new(
        DiagnosticKind::Type,
        DiagnosticCode::Uncaught,
        "outer failure",
    )
    .with_cause(inner.clone());

    let json = serde_json::to_string_pretty(&outer).expect("serialize chain");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("deserialize chain");
    let re = serde_json::to_string_pretty(&parsed).expect("re-serialize chain");
    assert_eq!(json, re);
    let parsed_cause = &parsed["cause"];
    assert!(parsed_cause.is_object(), "cause survived round-trip");
    assert_eq!(parsed_cause["code"], inner.code.as_str());
    assert_eq!(parsed_cause["message"], inner.message.as_str());
}
