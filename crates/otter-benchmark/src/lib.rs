//! Machine-readable benchmark records for the active Otter engine.
//!
//! # Contents
//! - [`BenchmarkResult`] is the one live, intentionally unversioned result
//!   contract.
//! - [`BenchmarkConfiguration`] records the exact VM/runtime/JIT/GC policy.
//! - [`Metric`] preserves raw samples and a typed aggregate.
//! - [`BenchmarkProvenance`] captures the source tree and host used to measure.
//!
//! # Invariants
//! - Contract changes are hard breaking; no aliases or compatibility readers
//!   are provided.
//! - A scoreable result is semantically validated and owns a primary metric.
//! - A checked-in baseline additionally requires a clean release build.
//! - A timeout is recorded only when that exact timeout was enforced.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub mod process;

/// Direct target calls used to seed arithmetic feedback before an engine
/// compile benchmark snapshots the function.
pub const ENGINE_COMPILE_FEEDBACK_SEED_CALLS: u32 = 8;

/// Stable workload identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkIdentity {
    /// Suite owning the workload, for example `engine` or `v8-v7`.
    pub suite: String,
    /// Stable workload name within the suite.
    pub name: String,
    /// Ordered workload parameters which materially change the measurement.
    pub parameters: BTreeMap<String, String>,
}

/// Host platform used for a measurement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Platform {
    /// Rust target operating-system name.
    pub os: String,
    /// Rust target architecture name.
    pub arch: String,
    /// Host kernel identity.
    pub kernel: String,
    /// Host CPU model when discoverable.
    pub cpu: String,
}

/// Source-tree, toolchain, build, and capture identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkProvenance {
    /// Unix timestamp in milliseconds.
    pub captured_at_unix_ms: u64,
    /// Full source commit, or `unknown` when Git cannot be queried.
    pub commit: String,
    /// Conservatively true when cleanliness cannot be established.
    pub dirty: bool,
    /// Host platform identity.
    pub platform: Platform,
    /// Full `rustc -Vv` output.
    pub rust_toolchain: String,
    /// Cargo profile used to build the measured executable.
    pub build_profile: String,
}

impl BenchmarkProvenance {
    /// Capture provenance after the timed region has completed.
    #[must_use]
    pub fn capture(build_profile: impl Into<String>) -> Self {
        let status = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .output();
        let dirty = match status {
            Ok(output) if output.status.success() => !output.stdout.is_empty(),
            _ => true,
        };
        Self {
            captured_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| {
                    duration
                        .as_millis()
                        .min(u128::from(u64::MAX))
                        .try_into()
                        .unwrap_or(u64::MAX)
                })
                .unwrap_or(0),
            commit: command_text("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into()),
            dirty,
            platform: Platform {
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                kernel: command_text("uname", &["-sr"]).unwrap_or_else(|| "unknown".into()),
                cpu: cpu_model(),
            },
            rust_toolchain: command_text("rustc", &["-Vv"]).unwrap_or_else(|| "unknown".into()),
            build_profile: build_profile.into(),
        }
    }
}

/// Build profile of the current benchmark process.
#[must_use]
pub const fn current_build_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn command_text(command: &str, args: &[&str]) -> Option<String> {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|output| !output.is_empty())
}

fn cpu_model() -> String {
    #[cfg(target_os = "macos")]
    if let Some(model) = command_text("sysctl", &["-n", "machdep.cpu.brand_string"]) {
        return model;
    }

    #[cfg(target_os = "linux")]
    if let Some(output) = command_text("lscpu", &[]) {
        if let Some(model) = output.lines().find_map(|line| {
            line.strip_prefix("Model name:")
                .map(str::trim)
                .filter(|model| !model.is_empty())
        }) {
            return model.into();
        }
    }

    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| std::env::consts::ARCH.into())
}

/// Execution surface owning the timed work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionSurface {
    /// In-process VM/interpreter API.
    Vm,
    /// In-process public runtime API.
    Runtime,
    /// Spawned Otter CLI process.
    CliProcess,
    /// Spawned non-Otter or otherwise external process.
    ExternalProcess,
}

/// Exact native-tier policy selected for a result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JitPolicy {
    /// Interpreter only.
    Interpreter,
    /// Template tier without optimizing compilation.
    Template,
    /// Optimizing compiler in isolation, without template fallback.
    Optimizing,
    /// Production template plus optimizing tier policy.
    ProductionTiered,
    /// No Otter JIT policy participates.
    NotApplicable,
}

/// Collector policy selected for a result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GcPolicy {
    /// Normal collector scheduling.
    Normal,
    /// Allocation-stride stress collection.
    Stress,
    /// Explicit minor collections.
    ForcedMinor,
    /// Explicit full collections.
    ForcedFull,
    /// No Otter collector participates.
    NotApplicable,
}

/// Runtime lifetime across measured samples.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeReuse {
    /// Construct a fresh runtime for every measured sample.
    FreshPerSample,
    /// Reuse one runtime across measured samples.
    ReusedAcrossSamples,
    /// No runtime lifetime participates.
    NotApplicable,
}

/// Input/cache state when a cache materially participates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheState {
    /// Applicable caches start empty.
    Cold,
    /// Applicable caches are populated before measurement.
    Warm,
    /// No cache is part of the workload contract.
    NotApplicable,
}

/// Execution configuration which must match before results are compared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkConfiguration {
    /// Surface owning the measured work.
    pub surface: ExecutionSurface,
    /// Exact JIT policy.
    pub jit_policy: JitPolicy,
    /// Explicit OSR threshold override.
    pub jit_osr_threshold: Option<u32>,
    /// Collector policy.
    pub gc_policy: GcPolicy,
    /// Stress allocation stride when stress policy is active.
    pub gc_stress_stride: Option<u32>,
    /// Runtime lifetime across samples.
    pub runtime_reuse: RuntimeReuse,
    /// Applicable cache state.
    pub cache_state: CacheState,
}

/// Sampling protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SamplingPlan {
    /// Untimed warmup executions.
    pub warmup_count: u32,
    /// Number of raw measured executions.
    pub sample_count: u32,
    /// Inner workload iterations performed by each sample.
    pub iterations_per_sample: Option<u64>,
    /// Enforced wall-clock cap, or null when none was enforced.
    pub timeout_ms: Option<u64>,
}

/// Numeric metric value preserving integral counters exactly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MetricValue {
    /// Exact integral counter.
    Integer(u64),
    /// Finite decimal measurement.
    Decimal(f64),
}

impl MetricValue {
    /// Convert either representation to `f64` for comparisons.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Integer(value) => value as f64,
            Self::Decimal(value) => value,
        }
    }
}

/// Metric unit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MetricUnit {
    /// Nanoseconds.
    Nanoseconds,
    /// Milliseconds.
    Milliseconds,
    /// Bytes.
    Bytes,
    /// Dimensionless count.
    Count,
    /// Suite-defined score.
    Score,
    /// Operations per second.
    OperationsPerSecond,
    /// Dimensionless ratio.
    Ratio,
    /// Percentage.
    Percent,
}

/// Whether higher or lower values are preferable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MetricDirection {
    /// Smaller values are preferable.
    LowerIsBetter,
    /// Larger values are preferable.
    HigherIsBetter,
    /// Metric is diagnostic and has no preferred direction.
    Informational,
}

/// Metric use in a comparison.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MetricRole {
    /// Main comparison metric.
    Primary,
    /// Supporting comparison metric.
    Secondary,
    /// Diagnostic metric not used for ranking.
    Diagnostic,
}

/// Aggregate statistic applied to raw samples.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Statistic {
    /// One raw sample, unchanged.
    Single,
    /// Median of raw samples.
    Median,
    /// Arithmetic mean.
    ArithmeticMean,
    /// Geometric mean.
    GeometricMean,
    /// Minimum sample.
    Minimum,
    /// Maximum sample.
    Maximum,
}

/// Aggregate paired with the exact statistic that produced it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricAggregate {
    /// Statistic applied to the raw samples.
    pub statistic: Statistic,
    /// Aggregate value.
    pub value: MetricValue,
}

/// Raw samples and their typed aggregate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metric {
    /// Stable metric identifier.
    pub name: String,
    /// Numeric unit.
    pub unit: MetricUnit,
    /// Preferred comparison direction.
    pub direction: MetricDirection,
    /// Comparison role.
    pub role: MetricRole,
    /// Raw measurements in execution order.
    pub samples: Vec<MetricValue>,
    /// Aggregate derived from `samples`.
    pub aggregate: MetricAggregate,
}

impl Metric {
    /// Build a metric from exact unsigned samples.
    pub fn from_u64_samples(
        name: impl Into<String>,
        unit: MetricUnit,
        direction: MetricDirection,
        role: MetricRole,
        samples: Vec<u64>,
        statistic: Statistic,
    ) -> Result<Self, String> {
        let aggregate = aggregate_u64(&samples, statistic)?;
        Ok(Self {
            name: name.into(),
            unit,
            direction,
            role,
            samples: samples.into_iter().map(MetricValue::Integer).collect(),
            aggregate: MetricAggregate {
                statistic,
                value: aggregate,
            },
        })
    }

    /// Build a metric from finite decimal samples.
    pub fn from_decimal_samples(
        name: impl Into<String>,
        unit: MetricUnit,
        direction: MetricDirection,
        role: MetricRole,
        samples: Vec<f64>,
        statistic: Statistic,
    ) -> Result<Self, String> {
        if samples.iter().any(|value| !value.is_finite()) {
            return Err("metric samples must be finite".into());
        }
        let aggregate = aggregate_f64(&samples, statistic)?;
        Ok(Self {
            name: name.into(),
            unit,
            direction,
            role,
            samples: samples.into_iter().map(MetricValue::Decimal).collect(),
            aggregate: MetricAggregate {
                statistic,
                value: MetricValue::Decimal(aggregate),
            },
        })
    }
}

fn aggregate_u64(samples: &[u64], statistic: Statistic) -> Result<MetricValue, String> {
    if samples.is_empty() {
        return Err("metric requires at least one sample".into());
    }
    let mut ordered = samples.to_vec();
    match statistic {
        Statistic::Single if samples.len() == 1 => Ok(MetricValue::Integer(samples[0])),
        Statistic::Single => Err("single statistic requires exactly one sample".into()),
        Statistic::Median => {
            ordered.sort_unstable();
            let middle = ordered.len() / 2;
            if ordered.len().is_multiple_of(2) {
                let sum = u128::from(ordered[middle - 1]) + u128::from(ordered[middle]);
                if sum.is_multiple_of(2) {
                    Ok(MetricValue::Integer((sum / 2) as u64))
                } else {
                    Ok(MetricValue::Decimal(sum as f64 / 2.0))
                }
            } else {
                Ok(MetricValue::Integer(ordered[middle]))
            }
        }
        Statistic::Minimum => Ok(MetricValue::Integer(*ordered.iter().min().unwrap())),
        Statistic::Maximum => Ok(MetricValue::Integer(*ordered.iter().max().unwrap())),
        Statistic::ArithmeticMean => Ok(MetricValue::Decimal(
            ordered.iter().map(|value| *value as f64).sum::<f64>() / ordered.len() as f64,
        )),
        Statistic::GeometricMean => aggregate_f64(
            &ordered
                .iter()
                .map(|value| *value as f64)
                .collect::<Vec<_>>(),
            statistic,
        )
        .map(MetricValue::Decimal),
    }
}

fn aggregate_f64(samples: &[f64], statistic: Statistic) -> Result<f64, String> {
    if samples.is_empty() {
        return Err("metric requires at least one sample".into());
    }
    match statistic {
        Statistic::Single if samples.len() == 1 => Ok(samples[0]),
        Statistic::Single => Err("single statistic requires exactly one sample".into()),
        Statistic::Median => {
            let mut ordered = samples.to_vec();
            ordered.sort_by(f64::total_cmp);
            let middle = ordered.len() / 2;
            if ordered.len().is_multiple_of(2) {
                Ok((ordered[middle - 1] + ordered[middle]) / 2.0)
            } else {
                Ok(ordered[middle])
            }
        }
        Statistic::ArithmeticMean => Ok(samples.iter().sum::<f64>() / samples.len() as f64),
        Statistic::GeometricMean => {
            if samples.iter().any(|value| *value <= 0.0) {
                return Err("geometric mean requires positive samples".into());
            }
            Ok((samples.iter().map(|value| value.ln()).sum::<f64>() / samples.len() as f64).exp())
        }
        Statistic::Minimum => Ok(*samples.iter().min_by(|a, b| a.total_cmp(b)).unwrap()),
        Statistic::Maximum => Ok(*samples.iter().max_by(|a, b| a.total_cmp(b)).unwrap()),
    }
}

fn recompute_metric_aggregate(metric: &Metric) -> Result<MetricValue, String> {
    let mut integer_samples = Vec::with_capacity(metric.samples.len());
    let mut decimal_samples = Vec::with_capacity(metric.samples.len());
    for sample in &metric.samples {
        match *sample {
            MetricValue::Integer(value) if decimal_samples.is_empty() => {
                integer_samples.push(value);
            }
            MetricValue::Decimal(value) if integer_samples.is_empty() && value.is_finite() => {
                decimal_samples.push(value);
            }
            MetricValue::Decimal(_) if !integer_samples.is_empty() => {
                return Err("integer and decimal samples cannot be mixed".into());
            }
            MetricValue::Integer(_) if !decimal_samples.is_empty() => {
                return Err("integer and decimal samples cannot be mixed".into());
            }
            MetricValue::Decimal(_) => return Err("decimal samples must be finite".into()),
            MetricValue::Integer(_) => {
                return Err("integer and decimal samples cannot be mixed".into());
            }
        }
    }
    if decimal_samples.is_empty() {
        aggregate_u64(&integer_samples, metric.aggregate.statistic)
    } else {
        aggregate_f64(&decimal_samples, metric.aggregate.statistic).map(MetricValue::Decimal)
    }
}

/// Failure classification.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// Invalid benchmark configuration.
    Configuration,
    /// Missing or malformed workload input.
    Input,
    /// Frontend or native compilation failure.
    Compile,
    /// VM/runtime execution failure.
    Runtime,
    /// Semantic validation failure.
    Validation,
    /// Enforced timeout expired.
    Timeout,
    /// Child-process spawn or exit failure.
    Process,
}

/// Owned failure detail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkFailure {
    /// Stable failure category.
    pub kind: FailureKind,
    /// Owned diagnostic detail.
    pub message: String,
}

/// Semantic/process outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeStatus {
    /// Workload completed and its semantic marker matched.
    Validated,
    /// Workload completed but no semantic marker was requested.
    Unvalidated,
    /// Workload or validation failed.
    Failed,
    /// Enforced timeout expired.
    TimedOut,
}

/// Workload outcome. In-process engine runs leave `process_exit_code` null.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkOutcome {
    /// Final outcome classification.
    pub status: OutcomeStatus,
    /// Observed semantic marker.
    pub validation_marker: Option<String>,
    /// Child-process exit code; null for in-process work or signals.
    pub process_exit_code: Option<i32>,
    /// Failure detail for failed/timed-out outcomes.
    pub failure: Option<BenchmarkFailure>,
}

/// One self-contained benchmark observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BenchmarkResult {
    /// Workload identity and parameters.
    pub benchmark: BenchmarkIdentity,
    /// Source, host, toolchain, and build identity.
    pub provenance: BenchmarkProvenance,
    /// VM/runtime/JIT/GC/cache policy.
    pub configuration: BenchmarkConfiguration,
    /// Warmup, sample, iteration, and timeout protocol.
    pub sampling: SamplingPlan,
    /// Raw measurements and aggregates.
    pub metrics: Vec<Metric>,
    /// Semantic/process outcome.
    pub outcome: BenchmarkOutcome,
    /// Exact command argv used for reproduction.
    pub command: Vec<String>,
}

impl BenchmarkResult {
    /// Whether this observation owns a validated primary performance metric.
    #[must_use]
    pub fn is_scoreable(&self) -> bool {
        self.outcome.status == OutcomeStatus::Validated && self.contract_error().is_none()
    }

    /// Whether this observation can enter a checked-in baseline.
    #[must_use]
    pub fn is_baseline_eligible(&self) -> bool {
        self.is_scoreable()
            && !self.provenance.dirty
            && self.provenance.build_profile == "release"
            && self.contract_error().is_none()
    }

    /// Return a structural contract error, if any.
    #[must_use]
    pub fn contract_error(&self) -> Option<String> {
        if self.sampling.sample_count == 0 {
            return Some("sampleCount must be greater than zero".into());
        }
        if self.command.is_empty() {
            return Some("command must contain at least one argv element".into());
        }
        match self.outcome.status {
            OutcomeStatus::Validated => {
                if self
                    .outcome
                    .validation_marker
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    return Some("validated result requires a validation marker".into());
                }
                if self.outcome.failure.is_some() {
                    return Some("validated result cannot carry a failure".into());
                }
            }
            OutcomeStatus::Unvalidated => {
                if self.outcome.validation_marker.is_some() || self.outcome.failure.is_some() {
                    return Some(
                        "unvalidated result cannot carry a validation marker or failure".into(),
                    );
                }
            }
            OutcomeStatus::Failed => {
                if self.outcome.validation_marker.is_some() || self.outcome.failure.is_none() {
                    return Some("failed result requires one failure and no marker".into());
                }
            }
            OutcomeStatus::TimedOut => {
                if self.sampling.timeout_ms.is_none()
                    || self.outcome.validation_marker.is_some()
                    || !self
                        .outcome
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.kind == FailureKind::Timeout)
                {
                    return Some(
                        "timed-out result requires an enforced timeout and timeout failure".into(),
                    );
                }
            }
        }
        let mut names = BTreeSet::new();
        let mut primary_count = 0usize;
        for metric in &self.metrics {
            let is_run_level_diagnostic = metric.role == MetricRole::Diagnostic
                && metric.aggregate.statistic == Statistic::Single
                && metric.samples.len() == 1;
            if !is_run_level_diagnostic
                && metric.samples.len() != self.sampling.sample_count as usize
            {
                return Some(format!(
                    "metric {:?} has {} samples, expected {}",
                    metric.name,
                    metric.samples.len(),
                    self.sampling.sample_count
                ));
            }
            if !names.insert(metric.name.as_str()) {
                return Some(format!("duplicate metric name {:?}", metric.name));
            }
            if metric.role == MetricRole::Primary {
                primary_count += 1;
            }
            let expected = match recompute_metric_aggregate(metric) {
                Ok(expected) => expected,
                Err(error) => {
                    return Some(format!("metric {:?}: {error}", metric.name));
                }
            };
            if metric.aggregate.value != expected {
                return Some(format!(
                    "metric {:?} aggregate {:?} does not match {:?} over raw samples",
                    metric.name, metric.aggregate.value, expected
                ));
            }
        }
        if primary_count > 1 {
            return Some(format!(
                "result has {primary_count} primary metrics, expected at most one"
            ));
        }
        if self.outcome.status == OutcomeStatus::Validated && primary_count != 1 {
            return Some(format!(
                "validated result has {primary_count} primary metrics, expected exactly one"
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result() -> BenchmarkResult {
        BenchmarkResult {
            benchmark: BenchmarkIdentity {
                suite: "engine".into(),
                name: "call".into(),
                parameters: BTreeMap::from([
                    ("arity".into(), "4".into()),
                    ("kind".into(), "bytecode".into()),
                ]),
            },
            provenance: BenchmarkProvenance {
                captured_at_unix_ms: 7,
                commit: "deadbeef".into(),
                dirty: false,
                platform: Platform {
                    os: "macos".into(),
                    arch: "aarch64".into(),
                    kernel: "Darwin".into(),
                    cpu: "test".into(),
                },
                rust_toolchain: "rustc test".into(),
                build_profile: "release".into(),
            },
            configuration: BenchmarkConfiguration {
                surface: ExecutionSurface::Vm,
                jit_policy: JitPolicy::ProductionTiered,
                jit_osr_threshold: None,
                gc_policy: GcPolicy::Normal,
                gc_stress_stride: None,
                runtime_reuse: RuntimeReuse::ReusedAcrossSamples,
                cache_state: CacheState::NotApplicable,
            },
            sampling: SamplingPlan {
                warmup_count: 3,
                sample_count: 3,
                iterations_per_sample: Some(10_000),
                timeout_ms: None,
            },
            metrics: vec![
                Metric::from_u64_samples(
                    "execution-time",
                    MetricUnit::Nanoseconds,
                    MetricDirection::LowerIsBetter,
                    MetricRole::Primary,
                    vec![9, 3, 6],
                    Statistic::Median,
                )
                .unwrap(),
            ],
            outcome: BenchmarkOutcome {
                status: OutcomeStatus::Validated,
                validation_marker: Some("return=10000".into()),
                process_exit_code: None,
                failure: None,
            },
            command: vec!["otter-engine-benchmark".into(), "call".into()],
        }
    }

    #[test]
    fn one_live_contract_is_camel_case_and_unversioned() {
        let json = serde_json::to_value(sample_result()).unwrap();
        assert_eq!(json["benchmark"]["parameters"]["arity"], "4");
        assert_eq!(json["configuration"]["jitPolicy"], "production-tiered");
        assert_eq!(json["sampling"]["sampleCount"], 3);
        assert_eq!(json["metrics"][0]["aggregate"]["value"], 6);
        assert!(json.get("schemaVersion").is_none());
        assert!(json.get("schema_version").is_none());
    }

    #[test]
    fn old_flat_contract_has_no_compatibility_reader() {
        let old = serde_json::json!({
            "benchmark": "smoke",
            "commit": "deadbeef",
            "runtime_mode": "vm",
            "success": true
        });
        assert!(serde_json::from_value::<BenchmarkResult>(old).is_err());
    }

    #[test]
    fn jit_policy_spellings_are_current() {
        assert_eq!(
            serde_json::to_string(&JitPolicy::Interpreter).unwrap(),
            "\"interpreter\""
        );
        assert_eq!(
            serde_json::to_string(&JitPolicy::Template).unwrap(),
            "\"template\""
        );
        assert_eq!(
            serde_json::to_string(&JitPolicy::Optimizing).unwrap(),
            "\"optimizing\""
        );
        assert_eq!(
            serde_json::to_string(&JitPolicy::ProductionTiered).unwrap(),
            "\"production-tiered\""
        );
        assert!(serde_json::from_str::<JitPolicy>("\"baseline\"").is_err());
        assert!(serde_json::from_str::<JitPolicy>("\"forced-baseline\"").is_err());
        assert!(serde_json::from_str::<JitPolicy>("\"experimental-optimizer\"").is_err());
    }

    #[test]
    fn raw_samples_and_median_are_preserved() {
        let result = sample_result();
        assert_eq!(
            result.metrics[0].samples,
            vec![
                MetricValue::Integer(9),
                MetricValue::Integer(3),
                MetricValue::Integer(6)
            ]
        );
        assert_eq!(
            result.metrics[0].aggregate,
            MetricAggregate {
                statistic: Statistic::Median,
                value: MetricValue::Integer(6),
            }
        );
    }

    #[test]
    fn even_integer_median_preserves_half_units() {
        let metric = Metric::from_u64_samples(
            "latency",
            MetricUnit::Nanoseconds,
            MetricDirection::LowerIsBetter,
            MetricRole::Primary,
            vec![1, 2],
            Statistic::Median,
        )
        .unwrap();
        assert_eq!(metric.aggregate.value, MetricValue::Decimal(1.5));
    }

    #[test]
    fn validated_result_without_primary_metric_is_not_scoreable() {
        let mut result = sample_result();
        result.metrics[0].role = MetricRole::Diagnostic;
        assert!(!result.is_scoreable());
    }

    #[test]
    fn dirty_or_debug_results_are_not_baseline_eligible() {
        let mut result = sample_result();
        assert!(result.is_baseline_eligible());
        result.provenance.dirty = true;
        assert!(result.is_scoreable());
        assert!(!result.is_baseline_eligible());
        result.provenance.dirty = false;
        result.provenance.build_profile = "debug".into();
        assert!(!result.is_baseline_eligible());
    }

    #[test]
    fn contract_rejects_duplicate_or_misaligned_metrics() {
        let mut result = sample_result();
        result.metrics.push(result.metrics[0].clone());
        assert!(result.contract_error().unwrap().contains("duplicate"));
        result.metrics.pop();
        result.metrics.push(
            Metric::from_u64_samples(
                "run-level-counter",
                MetricUnit::Count,
                MetricDirection::Informational,
                MetricRole::Diagnostic,
                vec![7],
                Statistic::Single,
            )
            .unwrap(),
        );
        assert_eq!(result.contract_error(), None);
        result.metrics.pop();
        result.metrics[0].samples.pop();
        assert!(result.contract_error().unwrap().contains("expected 3"));
    }

    #[test]
    fn contract_rejects_forged_aggregates_and_multiple_primaries() {
        let mut result = sample_result();
        result.metrics[0].aggregate.value = MetricValue::Integer(7);
        assert!(result.contract_error().unwrap().contains("does not match"));

        let mut result = sample_result();
        let mut second = result.metrics[0].clone();
        second.name = "second-primary".into();
        result.metrics.push(second);
        assert!(result.contract_error().unwrap().contains("primary metrics"));
        assert!(!result.is_scoreable());
        assert!(!result.is_baseline_eligible());
    }

    #[test]
    fn contract_rejects_forged_outcome_state() {
        let mut result = sample_result();
        result.outcome.validation_marker = None;
        assert!(
            result
                .contract_error()
                .unwrap()
                .contains("validation marker")
        );
        assert!(!result.is_scoreable());

        let mut result = sample_result();
        result.outcome.status = OutcomeStatus::TimedOut;
        result.outcome.validation_marker = None;
        result.outcome.failure = Some(BenchmarkFailure {
            kind: FailureKind::Timeout,
            message: "late".into(),
        });
        assert!(
            result
                .contract_error()
                .unwrap()
                .contains("enforced timeout")
        );
    }
}
