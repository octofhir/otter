//! Owned preparation and validation of complete persistent fixed-work Scripts.
//!
//! # Contents
//! - Seven immutable anchors, source-span manifests and common sampling DTOs.
//! - OXC-only recipes preserving global definitions and original useful checks.
//! - Strict nanosecond records from one realm and one captured monotonic clock.
//!
//! # Invariants
//! - No AST, arena, VM value or runtime handle crosses this boundary.
//! - Original algorithms/data remain byte copies; only driver lifetime changes.
//! - Reset, outer validation and output are outside every measurement window.
//! - Crypto requires an untimed original-result oracle before scoring.
//!
//! # See also
//! - [`crate::BenchmarkResult`] owns benchmark results, not this raw protocol.
//! - `otter_syntax::with_program_goal` owns all parsing.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Metric, MetricDirection, MetricRole, MetricUnit, SamplingPlan, Statistic};

mod ast;
mod emit;
mod recipes;
mod records;
#[cfg(test)]
mod tests;

/// All required fixed-work anchors; no reduced kernel is accepted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum WarmAnchor {
    /// Five recursive fib(30) calls.
    #[serde(rename = "fib")]
    Fib,
    /// Twenty million calls at one eight-receiver site.
    #[serde(rename = "mega_method")]
    MegaMethod,
    /// Two million full constructor chains.
    #[serde(rename = "ast_ctor")]
    AstCtor,
    /// Eight complete TypeScript compilation workloads.
    #[serde(rename = "ts")]
    Typescript,
    /// Sixty complete RSA encrypt/decrypt pairs.
    #[serde(rename = "crypto")]
    Crypto,
    /// Twelve zlib invocations with full-buffer integrity checks.
    #[serde(rename = "zlib")]
    Zlib,
    /// The entire deterministic Earley/Boyer suite.
    #[serde(rename = "earley-boyer")]
    EarleyBoyer,
}

impl WarmAnchor {
    /// Complete required coverage in deterministic order.
    pub const ALL: [Self; 7] = [
        Self::Fib,
        Self::MegaMethod,
        Self::AstCtor,
        Self::Typescript,
        Self::Crypto,
        Self::Zlib,
        Self::EarleyBoyer,
    ];

    /// Current anchor name, also used for generated filenames.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fib => "fib",
            Self::MegaMethod => "mega_method",
            Self::AstCtor => "ast_ctor",
            Self::Typescript => "ts",
            Self::Crypto => "crypto",
            Self::Zlib => "zlib",
            Self::EarleyBoyer => "earley-boyer",
        }
    }

    /// Original content identity predeclared before competitive measurement.
    pub const fn original_sha256(self) -> &'static str {
        match self {
            Self::Fib => "ca1b2f293556d0a4c9aa16ac405072aada33c01bb9fd0a92491d31ad94850d19",
            Self::MegaMethod => "1f75db87be075b4ad05298cd2941755b6ae5119f85dc8e47019ea9df73e85541",
            Self::AstCtor => "096f241237f7de047e15b27ee4bd0ac5687931726d808273777e99c4c4344988",
            Self::Typescript => "09a383b31c016cf303bfd0ec220c75efb35f8183a97cf1540ed0eda5913e2203",
            Self::Crypto => "01757736b2baa369b03c27a912e17e76209ed915f6c625ff0ab31090146d88c8",
            Self::Zlib => "a7061b7ee56180d0a7fe9ea630e60cda43ed0e366113b12678e86257e5435130",
            Self::EarleyBoyer => "401dfa05435e2ea7be4716ead24e93627485b363592ec92f860cc6248153b5e3",
        }
    }
}

/// Source and its declared original identity, all owned by the caller.
#[derive(Debug, Clone)]
pub struct WarmSource {
    /// Exact required anchor.
    pub anchor: WarmAnchor,
    /// Diagnostic source name; never used to locate a driver.
    pub source_name: String,
    /// Complete original Script bytes as UTF-8.
    pub original: String,
    /// Identity supplied by the frozen input manifest.
    pub expected_sha256: String,
}

/// Preparation input; workload iterations cannot be overridden.
#[derive(Debug, Clone)]
pub struct WarmHarnessRequest {
    /// The original complete fixture.
    pub source: WarmSource,
    /// At least three warm and five measured invocations in one process.
    pub sampling: SamplingPlan,
}

/// Purpose of a copied or inspected original source span.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WarmPieceRole {
    /// Script-level definitions, bindings, comments and data.
    Setup,
    /// The complete original fixed driver.
    Work,
    /// Original observable result expression.
    Result,
    /// Original statement replayed to reset deterministic state.
    ResetSource,
    /// Original cleanup statement.
    Cleanup,
    /// Original useful integrity checker retained unchanged.
    Integrity,
}

/// A byte span in the original source, or a separately identified embedded Script.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarmSourcePiece {
    /// Why this source was selected.
    pub role: WarmPieceRole,
    /// Inclusive original UTF-8 byte offset.
    pub start: u32,
    /// Exclusive original UTF-8 byte offset.
    pub end: u32,
    /// Exact selected bytes' content identity.
    pub sha256: String,
}

/// Decoded dynamic Script inspected through OXC, without rewriting its literal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarmEmbeddedScript {
    /// Original StringLiteral byte span.
    pub container: WarmSourcePiece,
    /// Decoded source content identity.
    pub decoded_sha256: String,
    /// Checked spans whose offsets refer to the decoded source.
    pub integrity: Vec<WarmSourcePiece>,
}

/// Useful result read after the timer, without lossy numeric coercion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum WarmSemanticResult {
    /// Analytically checked integral checksum.
    Integer(i64),
    /// Original textual result.
    Text(String),
    /// All original inner checks completed; outer checks are recorded separately.
    CheckedWork,
}

/// Observed outer-validation data, distinct from static workload claims.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum WarmCheckValue {
    /// An exact observed count.
    Integer(i64),
    /// A validated predicate.
    Boolean(bool),
    /// Observed textual metadata.
    Text(String),
}

/// Current exact generated source contract, with no compatibility versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarmHarnessManifest {
    /// Required anchor identity.
    pub anchor: WarmAnchor,
    /// Diagnostic original source name.
    pub source_name: String,
    /// Complete original source hash.
    pub original_sha256: String,
    /// Complete generated Script hash.
    pub generated_sha256: String,
    /// Normalized host prefix plus the full original source, for untimed oracles.
    pub original_script_sha256: String,
    /// Exact persistent invocation counts.
    pub sampling: SamplingPlan,
    /// Copied/inspected original source spans.
    pub pieces: Vec<WarmSourcePiece>,
    /// OXC-inspected decoded Scripts.
    pub embedded_scripts: Vec<WarmEmbeddedScript>,
    /// Immutable input data hashes.
    pub data_sha256: BTreeMap<String, String>,
    /// Static source-proven workload counts, never fabricated runtime observations.
    pub work_contract: BTreeMap<String, String>,
    /// Exact common clock contract.
    pub clock: String,
    /// Exact common measurement scope.
    pub scope: String,
    /// Emscripten host branch selected during untimed setup.
    pub host_environment: String,
    /// Crypto remains unscoreable until an untimed original oracle supplies this.
    pub expected_result: Option<WarmSemanticResult>,
    /// Outer predicates which every invocation must report as true.
    pub required_checks: BTreeMap<String, WarmCheckValue>,
}

/// Complete emitted Script and its source/validation contract.
#[derive(Debug, Clone)]
pub struct PreparedWarmHarness {
    /// Byte-identical common source for all engines.
    pub script: String,
    /// Exact common host prefix followed by the complete original Script.
    pub original_script: String,
    /// Owned span, data, sampling and result metadata.
    pub manifest: WarmHarnessManifest,
}

/// Excluded warm or measured invocation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WarmPhase {
    /// Full original work executed before measuring.
    Warmup,
    /// Full original work between monotonic timestamps.
    Measured,
}

/// One shared carrier for both legal invocation phases.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WarmInvocation {
    /// Which phase owns this observation.
    pub phase: WarmPhase,
    /// Zero-based position in that phase.
    pub index: u32,
    /// Exact unsigned decimal nanoseconds; null for excluded warmups.
    pub elapsed_ns_decimal: Option<String>,
    /// Actual result read after work.
    pub result: WarmSemanticResult,
    /// Actual outer validation predicates/counts.
    pub checks: BTreeMap<String, WarmCheckValue>,
}

/// One strict line protocol emitted by the common Script.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WarmRecord {
    /// Setup finished outside any timer.
    Ready {
        /// Exact source identity.
        anchor: WarmAnchor,
        /// Original source hash.
        original_sha256: String,
        /// Common measurement scope.
        scope: String,
        /// Captured monotonic clock contract.
        clock: String,
        /// Expected excluded invocations.
        warmup_count: u32,
        /// Expected measured invocations.
        sample_count: u32,
    },
    /// Actual full-work observation.
    Invocation(WarmInvocation),
    /// All work and final cleanup completed successfully.
    Complete {
        /// Exact required anchor.
        anchor: WarmAnchor,
        /// Completed excluded invocations.
        warmup_count: u32,
        /// Completed measured invocations.
        sample_count: u32,
    },
}

/// Successfully validated raw observations; raw output remains the driver's responsibility.
#[derive(Debug, Clone)]
pub struct ValidatedWarmRun {
    /// Exact required anchor.
    pub anchor: WarmAnchor,
    /// All excluded semantic observations.
    pub warmups: Vec<WarmInvocation>,
    /// All measured semantic observations.
    pub samples: Vec<WarmInvocation>,
    /// Exact parsed nanoseconds in execution order.
    pub measured_ns: Vec<u64>,
}

impl ValidatedWarmRun {
    /// Build the primary metric through the existing single result contract.
    pub fn metric(&self) -> Result<Metric, String> {
        Metric::from_u64_samples(
            "validated-work-seconds",
            MetricUnit::Nanoseconds,
            MetricDirection::LowerIsBetter,
            MetricRole::Primary,
            self.measured_ns.clone(),
            Statistic::Median,
        )
    }
}

/// Owned rejection of source geometry, sampling or raw observation data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmHarnessError {
    /// Concrete reason; includes relevant source/anchor geometry.
    pub message: String,
}

impl fmt::Display for WarmHarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for WarmHarnessError {}

pub(super) fn reject(message: impl Into<String>) -> WarmHarnessError {
    WarmHarnessError {
        message: message.into(),
    }
}

/// Exact SHA-256 used by source and wrapper manifests.
pub fn source_sha256(source: &[u8]) -> String {
    format!("{:x}", Sha256::digest(source))
}

/// Prepare all algorithm/data bytes through the current OXC Script surface.
pub fn prepare_warm_harness(
    request: WarmHarnessRequest,
) -> Result<PreparedWarmHarness, WarmHarnessError> {
    let sampling = &request.sampling;
    if sampling.warmup_count < 3
        || sampling.sample_count < 5
        || sampling.iterations_per_sample.is_some()
    {
        return Err(reject(
            "warm harness requires at least 3 warm + 5 measured complete invocations; no iteration override",
        ));
    }
    let actual = source_sha256(request.source.original.as_bytes());
    if request.source.expected_sha256 != request.source.anchor.original_sha256()
        || actual != request.source.expected_sha256
    {
        return Err(reject(format!(
            "{}: original anchor hash mismatch",
            request.source.source_name
        )));
    }
    let plan = recipes::prepare(&request.source)?;
    let script = emit::emit(&request, &plan)?;
    let original_script = emit::original(&request.source.original)?;
    ast::parse(&script, |_| Ok(()))?;
    let manifest = WarmHarnessManifest {
        anchor: request.source.anchor,
        source_name: request.source.source_name,
        original_sha256: actual,
        generated_sha256: source_sha256(script.as_bytes()),
        original_script_sha256: source_sha256(original_script.as_bytes()),
        sampling: request.sampling,
        pieces: plan.pieces,
        embedded_scripts: plan.embedded,
        data_sha256: plan.data,
        work_contract: plan.contract,
        clock: emit::CLOCK.into(),
        scope: emit::SCOPE.into(),
        host_environment: "classic-script-shell".into(),
        expected_result: plan.expected,
        required_checks: plan.required,
    };
    Ok(PreparedWarmHarness {
        script,
        original_script,
        manifest,
    })
}

/// Reject partial or mismatched records; crypto needs a bound original oracle.
pub fn validate_warm_records(
    manifest: &WarmHarnessManifest,
    stdout: &[u8],
) -> Result<ValidatedWarmRun, WarmHarnessError> {
    records::validate(manifest, stdout)
}
