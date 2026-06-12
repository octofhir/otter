//! `--json` round-trip parity for every plan-mandated diagnostic
//! category per ENGINE_REFACTOR_EXECUTION_PLAN §P2.3.
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
//! 3. deserializes the produced JSON back through `serde_json::from_str`;
//! 4. re-serializes the round-tripped value and asserts byte-identity
//!    against the original JSON.
//!
//! Byte-identical re-serialization pins the wire shape: any field
//! added without a `#[serde(skip_serializing_if = "Option::is_none")]`
//! escape hatch (or any rename) immediately fails.

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
        let parsed: Diagnostic = serde_json::from_str(&first).expect("deserialize diagnostic");
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
    // The envelope deserializes through a private wrapper in
    // `error.rs`; rebuild the wrapper here so we round-trip the
    // exact wire shape `--json` writes to stdout.
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct Envelope {
        error_schema_version: u32,
        error: OtterError,
    }
    let parsed: Envelope = serde_json::from_str(&first).expect("deserialize error envelope");
    assert_eq!(parsed.error_schema_version, 1);
    let second_inner = parsed.error.to_json_pretty().expect("re-serialize error");
    assert_eq!(first, second_inner, "OtterError JSON round-trip diverged");
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
    let parsed: Diagnostic = serde_json::from_str(&json).expect("deserialize chain");
    let re = serde_json::to_string_pretty(&parsed).expect("re-serialize chain");
    assert_eq!(json, re);
    let parsed_cause = parsed.cause.expect("cause survived round-trip");
    assert_eq!(parsed_cause.code, inner.code);
    assert_eq!(parsed_cause.message, inner.message);
}
