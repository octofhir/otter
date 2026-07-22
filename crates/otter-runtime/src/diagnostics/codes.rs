//! Stable diagnostic codes and categories for the runtime boundary.
//!
//! # Contents
//! - [`DiagnosticCode`] — closed enum of every diagnostic code the
//!   active runtime stack emits. Replaces ad-hoc string literals.
//! - [`DiagnosticCategory`] — the seven runtime categories
//!   (load / resolve / parse / compile / permission / runtime /
//!   package-manager) plus a residual `Internal` bucket for
//!   bug-class invariant violations.
//!
//! # Invariants
//! - Each variant has exactly one canonical `as_str()` text. The
//!   text is the wire-format diagnostic code that flows through
//!   [`crate::Diagnostic::code`] and the `--json` CLI surface.
//! - Code text is a non-empty ASCII identifier of `[A-Z0-9_]+` so
//!   it round-trips losslessly through JSON, YAML, and CLI args.
//! - Adding a code is a stable surface change and requires a new
//!   variant — never a free-form string.
//!
//! # See also
//! - [`crate::Diagnostic`]
//! - [`crate::OtterError`]

use serde::{Deserialize, Serialize};

/// Closed set of stable diagnostic codes.
///
/// Producers must not stamp ad-hoc strings on
/// [`crate::Diagnostic::code`] — every code reaches the public
/// surface through this enum. The associated `as_str()` method
/// returns the canonical wire-format code text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum DiagnosticCode {
    // ── Parse (otter-syntax / oxc frontend) ────────────────────
    /// Parser rejected the source (oxc diagnostic batch).
    SyntaxError,
    /// TypeScript construct outside the supported runtime subset
    /// (e.g. enums with computed members).
    TsUnsupported,
    /// AST node not yet lowered by the foundation compiler.
    FeatureNotInSlice,
    /// The regex engine rejected the regular expression source.
    InvalidRegexp,

    // ── Resolve (module loader, after capability gate) ─────────
    /// Loader could not resolve / fetch / read the import.
    ModuleResolutionError,
    /// Cycle or depth limit hit while linking the module graph.
    ModuleGraphCycle,

    // ── Permission (capability layer) ──────────────────────────
    /// Generic capability denial (host-API call without grant).
    CapabilityDenied,
    /// Module import denied because the matching `Net` / `FsRead`
    /// capability is not granted for the resolved resource.
    ModuleCapabilityDenied,

    // ── Compile (lowering after parse, before VM) ──────────────
    /// Compiler returned a variant the runtime mapper does not
    /// know about. Bug-class.
    CompileUnknown,

    // ── Runtime (VM-thrown) ────────────────────────────────────
    /// Operand types did not satisfy the opcode's contract.
    TypeMismatch,
    /// Catchable JS `TypeError`.
    TypeError,
    /// Method lookup failed on a built-in (e.g. `Math.foo`).
    UnknownMethod,
    /// Read of a let/const before its initializer ran.
    Tdz,
    /// `RangeError` from JS call-stack limit.
    StackOverflow,
    /// Operand was not callable.
    NotCallable,
    /// Catchable JS exception escaped the script.
    Uncaught,
    /// `JSON.stringify` saw a cycle.
    JsonCyclic,
    /// `JSON.stringify` saw a `BigInt`.
    JsonBigint,
    /// `JSON.stringify` exceeded the depth guard.
    JsonDepth,
    /// `JSON.parse` rejected the input.
    JsonParse,
    /// `JSON.parse` / `stringify` got a non-conforming argument.
    JsonBadArg,
    /// Microtask queue exceeded the host-set runaway guard.
    MicrotaskRunaway,
    /// Runtime budget rejected a VM turn.
    BudgetExceeded,

    // ── Package-manager (otter-pm-* surfaces) ──────────────────
    /// Manifest `name` field is empty.
    PmManifestEmptyName,
    /// Manifest `version` field is empty.
    PmManifestEmptyVersion,
    /// Manifest dependency map has an empty key.
    PmManifestEmptyDependencyName,
    /// Manifest dependency map has an empty range value.
    PmManifestEmptyDependencyRange,

    // ── Internal (bug-class — `Result<_, OtterError>::Internal`) ─
    /// VM produced a state the embedder could not represent.
    VmBytecodeInvariant,
    /// Catch-all for new VM error variants the mapper does not
    /// know about yet. Bug-class.
    VmUnknown,
    /// `install_global_class` failed during runtime construction.
    GlobalClassBootstrap,
    /// String allocation against the per-isolate string heap
    /// failed (typically heap cap exhaustion).
    StringAlloc,
    /// Microtask execution had no bytecode module available to
    /// resolve function ids against.
    MicrotaskDrainNeedsModule,
    /// Runner thread shut down before the command could be
    /// processed.
    RuntimeShutdown,
    /// Runtime command queue is at capacity (back-pressure).
    RuntimeBackpressure,
    /// Runtime command channel was closed by the runner.
    RuntimeClosed,
    /// Runtime runner dropped the oneshot reply channel.
    RuntimeReplyDropped,
    /// `IsolateRunner::spawn` failed (Tokio thread error).
    IsolateSpawn,
    /// `IsolateRunner::start` returned a fatal error.
    IsolateStart,
    /// Failed to bring up the worker-side Tokio runtime.
    TokioRuntimeCreate,
    /// CLI bytecode-dump JSON serialization failed.
    DumpJson,
}

impl DiagnosticCode {
    /// Canonical wire-format code text. The string matches the
    /// `code` field on [`crate::Diagnostic`] and the
    /// `--json`/serde representation of [`crate::OtterError`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        use DiagnosticCode::*;
        match self {
            SyntaxError => "SYNTAX_ERROR",
            TsUnsupported => "TS_UNSUPPORTED",
            FeatureNotInSlice => "FEATURE_NOT_IN_SLICE",
            InvalidRegexp => "INVALID_REGEXP",
            ModuleResolutionError => "MODULE_RESOLUTION_ERROR",
            ModuleGraphCycle => "MODULE_GRAPH_CYCLE",
            CapabilityDenied => "CAPABILITY_DENIED",
            ModuleCapabilityDenied => "MODULE_CAPABILITY_DENIED",
            CompileUnknown => "COMPILE_UNKNOWN",
            TypeMismatch => "TYPE_MISMATCH",
            TypeError => "TYPE_ERROR",
            UnknownMethod => "UNKNOWN_METHOD",
            Tdz => "TDZ",
            StackOverflow => "STACK_OVERFLOW",
            NotCallable => "NOT_CALLABLE",
            Uncaught => "UNCAUGHT",
            JsonCyclic => "JSON_CYCLIC",
            JsonBigint => "JSON_BIGINT",
            JsonDepth => "JSON_DEPTH",
            JsonParse => "JSON_PARSE",
            JsonBadArg => "JSON_BAD_ARG",
            MicrotaskRunaway => "MICROTASK_RUNAWAY",
            BudgetExceeded => "BUDGET_EXCEEDED",
            PmManifestEmptyName => "PM_MANIFEST_EMPTY_NAME",
            PmManifestEmptyVersion => "PM_MANIFEST_EMPTY_VERSION",
            PmManifestEmptyDependencyName => "PM_MANIFEST_EMPTY_DEPENDENCY_NAME",
            PmManifestEmptyDependencyRange => "PM_MANIFEST_EMPTY_DEPENDENCY_RANGE",
            VmBytecodeInvariant => "VM_BYTECODE_INVARIANT",
            VmUnknown => "VM_UNKNOWN",
            GlobalClassBootstrap => "GLOBAL_CLASS_BOOTSTRAP",
            StringAlloc => "STRING_ALLOC",
            MicrotaskDrainNeedsModule => "MICROTASK_DRAIN_NEEDS_MODULE",
            RuntimeShutdown => "RUNTIME_SHUTDOWN",
            RuntimeBackpressure => "RUNTIME_BACKPRESSURE",
            RuntimeClosed => "RUNTIME_CLOSED",
            RuntimeReplyDropped => "RUNTIME_REPLY_DROPPED",
            IsolateSpawn => "ISOLATE_SPAWN",
            IsolateStart => "ISOLATE_START",
            TokioRuntimeCreate => "TOKIO_RUNTIME_CREATE",
            DumpJson => "DUMP_JSON",
        }
    }

    /// Plan §P2.3 category for this code. Used by tooling that
    /// wants to bucket diagnostics without inspecting the
    /// individual variant.
    #[must_use]
    pub const fn category(self) -> DiagnosticCategory {
        use DiagnosticCategory as Cat;
        use DiagnosticCode::*;
        match self {
            SyntaxError | TsUnsupported | FeatureNotInSlice | InvalidRegexp => Cat::Parse,
            ModuleResolutionError | ModuleGraphCycle => Cat::Resolve,
            CapabilityDenied | ModuleCapabilityDenied => Cat::Permission,
            CompileUnknown => Cat::Compile,
            TypeMismatch | TypeError | UnknownMethod | Tdz | StackOverflow | NotCallable
            | Uncaught | JsonCyclic | JsonBigint | JsonDepth | JsonParse | JsonBadArg
            | MicrotaskRunaway | BudgetExceeded => Cat::Runtime,
            PmManifestEmptyName
            | PmManifestEmptyVersion
            | PmManifestEmptyDependencyName
            | PmManifestEmptyDependencyRange => Cat::PackageManager,
            VmBytecodeInvariant
            | VmUnknown
            | GlobalClassBootstrap
            | StringAlloc
            | MicrotaskDrainNeedsModule
            | RuntimeShutdown
            | RuntimeBackpressure
            | RuntimeClosed
            | RuntimeReplyDropped
            | IsolateSpawn
            | IsolateStart
            | TokioRuntimeCreate
            | DumpJson => Cat::Internal,
        }
    }

    /// Parse a wire-format code string back to the enum. Returns
    /// `None` for any string outside the closed set.
    ///
    /// Named `parse` rather than `from_str` so the closed-set
    /// semantics (returning `Option`, no error type) stay
    /// distinct from the `std::str::FromStr` trait contract.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        use DiagnosticCode::*;
        Some(match text {
            "SYNTAX_ERROR" => SyntaxError,
            "TS_UNSUPPORTED" => TsUnsupported,
            "FEATURE_NOT_IN_SLICE" => FeatureNotInSlice,
            "INVALID_REGEXP" => InvalidRegexp,
            "MODULE_RESOLUTION_ERROR" => ModuleResolutionError,
            "MODULE_GRAPH_CYCLE" => ModuleGraphCycle,
            "CAPABILITY_DENIED" => CapabilityDenied,
            "MODULE_CAPABILITY_DENIED" => ModuleCapabilityDenied,
            "COMPILE_UNKNOWN" => CompileUnknown,
            "TYPE_MISMATCH" => TypeMismatch,
            "TYPE_ERROR" => TypeError,
            "UNKNOWN_METHOD" => UnknownMethod,
            "TDZ" => Tdz,
            "STACK_OVERFLOW" => StackOverflow,
            "NOT_CALLABLE" => NotCallable,
            "UNCAUGHT" => Uncaught,
            "JSON_CYCLIC" => JsonCyclic,
            "JSON_BIGINT" => JsonBigint,
            "JSON_DEPTH" => JsonDepth,
            "JSON_PARSE" => JsonParse,
            "JSON_BAD_ARG" => JsonBadArg,
            "MICROTASK_RUNAWAY" => MicrotaskRunaway,
            "BUDGET_EXCEEDED" => BudgetExceeded,
            "PM_MANIFEST_EMPTY_NAME" => PmManifestEmptyName,
            "PM_MANIFEST_EMPTY_VERSION" => PmManifestEmptyVersion,
            "PM_MANIFEST_EMPTY_DEPENDENCY_NAME" => PmManifestEmptyDependencyName,
            "PM_MANIFEST_EMPTY_DEPENDENCY_RANGE" => PmManifestEmptyDependencyRange,
            "VM_BYTECODE_INVARIANT" => VmBytecodeInvariant,
            "VM_UNKNOWN" => VmUnknown,
            "GLOBAL_CLASS_BOOTSTRAP" => GlobalClassBootstrap,
            "STRING_ALLOC" => StringAlloc,
            "MICROTASK_DRAIN_NEEDS_MODULE" => MicrotaskDrainNeedsModule,
            "RUNTIME_SHUTDOWN" => RuntimeShutdown,
            "RUNTIME_BACKPRESSURE" => RuntimeBackpressure,
            "RUNTIME_CLOSED" => RuntimeClosed,
            "RUNTIME_REPLY_DROPPED" => RuntimeReplyDropped,
            "ISOLATE_SPAWN" => IsolateSpawn,
            "ISOLATE_START" => IsolateStart,
            "TOKIO_RUNTIME_CREATE" => TokioRuntimeCreate,
            "DUMP_JSON" => DumpJson,
            _ => return None,
        })
    }

    /// Every variant in declaration order. Used by snapshot tests
    /// that audit invariants across the closed set.
    #[must_use]
    pub fn all() -> &'static [DiagnosticCode] {
        use DiagnosticCode::*;
        &[
            SyntaxError,
            TsUnsupported,
            FeatureNotInSlice,
            InvalidRegexp,
            ModuleResolutionError,
            ModuleGraphCycle,
            CapabilityDenied,
            ModuleCapabilityDenied,
            CompileUnknown,
            TypeMismatch,
            TypeError,
            UnknownMethod,
            Tdz,
            StackOverflow,
            NotCallable,
            Uncaught,
            JsonCyclic,
            JsonBigint,
            JsonDepth,
            JsonParse,
            JsonBadArg,
            MicrotaskRunaway,
            BudgetExceeded,
            PmManifestEmptyName,
            PmManifestEmptyVersion,
            PmManifestEmptyDependencyName,
            PmManifestEmptyDependencyRange,
            VmBytecodeInvariant,
            VmUnknown,
            GlobalClassBootstrap,
            StringAlloc,
            MicrotaskDrainNeedsModule,
            RuntimeShutdown,
            RuntimeBackpressure,
            RuntimeClosed,
            RuntimeReplyDropped,
            IsolateSpawn,
            IsolateStart,
            TokioRuntimeCreate,
            DumpJson,
        ]
    }
}

impl std::fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Plan §P2.3 diagnostic category. Coarser bucket than the
/// closed [`DiagnosticCode`] set — matches the seven categories
/// the diagnostics ADR mandates plus a residual bug-class
/// `Internal` bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DiagnosticCategory {
    /// Source loading (file/HTTPS read failures). Reserved — no
    /// codes routed here yet because the active loader collapses
    /// load + resolve into [`DiagnosticCode::ModuleResolutionError`].
    Load,
    /// Module specifier resolution / graph linking.
    Resolve,
    /// Parser (oxc) and TypeScript-erasure failures.
    Parse,
    /// Compiler / lowering failures past the parser.
    Compile,
    /// Capability layer denials.
    Permission,
    /// VM-thrown ECMAScript-semantics errors.
    Runtime,
    /// Package-manager manifests, lockfiles, and graphs.
    PackageManager,
    /// Bug-class internal invariant violations. Surfaces as
    /// [`crate::OtterError::Internal`].
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_variant_has_unique_code_text() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for code in DiagnosticCode::all() {
            let text = code.as_str();
            assert!(
                seen.insert(text),
                "duplicate diagnostic code text {text:?} for variant {code:?}"
            );
        }
    }

    #[test]
    fn code_text_is_uppercase_ascii_identifier() {
        for code in DiagnosticCode::all() {
            let text = code.as_str();
            assert!(!text.is_empty(), "empty code text for {code:?}");
            for c in text.chars() {
                assert!(
                    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_',
                    "non-conforming character {c:?} in code text {text:?} ({code:?})"
                );
            }
        }
    }

    #[test]
    fn from_str_round_trips() {
        for code in DiagnosticCode::all() {
            let text = code.as_str();
            let parsed = DiagnosticCode::parse(text)
                .unwrap_or_else(|| panic!("from_str rejected canonical text {text:?}"));
            assert_eq!(parsed, *code, "round-trip mismatch for {text:?}");
        }
    }

    #[test]
    fn from_str_rejects_unknown_text() {
        assert!(DiagnosticCode::parse("not_a_real_code").is_none());
        assert!(DiagnosticCode::parse("").is_none());
        assert!(DiagnosticCode::parse("syntax_error").is_none()); // case-sensitive
    }

    #[test]
    fn category_assignment_covers_every_variant() {
        for code in DiagnosticCode::all() {
            let _category = code.category();
        }
    }

    #[test]
    fn json_round_trip_uses_canonical_code_text() {
        let code = DiagnosticCode::ModuleCapabilityDenied;
        let json = serde_json::to_string(&code).unwrap();
        assert_eq!(json, "\"MODULE_CAPABILITY_DENIED\"");
        let parsed: DiagnosticCode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, code);
    }
}
