//! Machine-readable benchmark records for VM, runtime, CLI, and package gates.
//!
//! # Contents
//! - [`BenchmarkResult`] is the stable JSON envelope emitted by every Phase 0
//!   benchmark command.
//! - [`ExecutionMetrics`] and [`MemoryMetrics`] separate measured phase data
//!   from command identity and correctness validation.
//!
//! # Invariants
//! - Every record carries commit/platform/toolchain/mode/cache/correctness data.
//! - A workload without a positive validation marker has no valid performance
//!   score, even when its process exits successfully.
//! - Unavailable counters are serialized as `null`, never silently omitted.

use serde::{Deserialize, Serialize};

/// JSON schema version for [`BenchmarkResult`].
pub const BENCHMARK_RESULT_SCHEMA_VERSION: u32 = 1;

/// Runtime execution mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// Standalone CLI process.
    Cli,
    /// Embedded runtime call.
    Embed,
    /// VM-only benchmark.
    Vm,
    /// Module or package workload.
    Package,
}

/// JIT selection used by a result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum JitMode {
    /// No JIT hook installed.
    InterpreterOnly,
    /// Normal baseline tiering policy.
    Baseline,
    /// Thresholds configured to enter baseline as early as supported.
    ForcedBaseline,
    /// Pre-refactor optimizer, explicitly experimental.
    ExperimentalOptimizer,
}

/// GC policy used by a result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GcMode {
    /// Normal collector policy.
    Normal,
    /// Allocation-stride stress collection.
    Stress,
    /// Explicit minor collection injection.
    ForcedMinor,
    /// Explicit full collection injection.
    ForcedFull,
}

/// Input/cache state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CacheState {
    /// First process/run with caches absent or cleared.
    Cold,
    /// Repeated process/run with applicable caches populated.
    Warm,
    /// No cache participates in the benchmark.
    NotApplicable,
}

/// Correctness status. Only [`Self::Validated`] permits a performance score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationStatus {
    /// Workload emitted the expected success marker and exited successfully.
    Validated,
    /// Process succeeded but no semantic success marker was checked.
    Unvalidated,
    /// Workload failed or its semantic marker was absent.
    Failed,
}

/// Phase and generated-code measurements in nanoseconds/bytes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionMetrics {
    /// End-to-end wall-clock time.
    pub wall_time_ns: u64,
    /// JS execution time, when separately available.
    pub execution_time_ns: Option<u64>,
    /// Parse time.
    pub parse_time_ns: Option<u64>,
    /// Compile/CodeBlock construction time.
    pub compile_time_ns: Option<u64>,
    /// Module resolve time.
    pub resolve_time_ns: Option<u64>,
    /// Module load time.
    pub load_time_ns: Option<u64>,
    /// Module link time.
    pub link_time_ns: Option<u64>,
    /// Runtime construction/bootstrap time.
    pub runtime_build_time_ns: Option<u64>,
    /// Generated native code bytes.
    pub code_bytes: Option<u64>,
}

/// Allocation, GC, heap, RSS, and code-memory measurements.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryMetrics {
    /// Managed/external allocation count.
    pub allocations: Option<u64>,
    /// Total measured GC time.
    pub gc_time_ns: Option<u64>,
    /// Peak resident set size.
    pub peak_rss_bytes: Option<u64>,
    /// Peak/live managed heap bytes.
    pub heap_bytes: Option<u64>,
    /// Executable code memory bytes.
    pub code_memory_bytes: Option<u64>,
}

/// One benchmark observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchmarkResult {
    /// [`BENCHMARK_RESULT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Stable workload name.
    pub benchmark: String,
    /// Full Otter git commit.
    pub commit: String,
    /// OS/architecture/CPU description.
    pub platform: String,
    /// Rust toolchain identity.
    pub toolchain: String,
    /// Cargo build profile.
    pub build_profile: String,
    /// Runtime surface.
    pub runtime_mode: RuntimeMode,
    /// Tier/JIT policy.
    pub jit_mode: JitMode,
    /// GC policy.
    pub gc_mode: GcMode,
    /// GC stress stride, when applicable.
    pub gc_stress_stride: Option<u32>,
    /// Cache policy.
    pub cache_state: CacheState,
    /// Timings/code size.
    pub execution: ExecutionMetrics,
    /// Allocation/memory counters.
    pub memory: MemoryMetrics,
    /// Process exit code, or `null` when terminated by a signal.
    pub exit_code: Option<i32>,
    /// Success/failure classification.
    pub success: bool,
    /// Semantic validation state.
    pub validation: ValidationStatus,
    /// Marker that was required/observed, when applicable.
    pub validation_marker: Option<String>,
    /// Command argv used for reproduction.
    pub command: Vec<String>,
    /// Optional failure classification/details.
    pub failure: Option<String>,
}

impl BenchmarkResult {
    /// Whether this record may contribute a performance score.
    #[must_use]
    pub fn is_scoreable(&self) -> bool {
        self.success && self.validation == ValidationStatus::Validated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_metrics_remain_explicit_nulls() {
        let metrics = ExecutionMetrics {
            wall_time_ns: 7,
            ..ExecutionMetrics::default()
        };
        let json = serde_json::to_value(metrics).unwrap();
        assert_eq!(json["compile_time_ns"], serde_json::Value::Null);
        assert_eq!(json["code_bytes"], serde_json::Value::Null);
    }

    #[test]
    fn unvalidated_success_is_not_scoreable() {
        let result = BenchmarkResult {
            schema_version: BENCHMARK_RESULT_SCHEMA_VERSION,
            benchmark: "smoke".into(),
            commit: "deadbeef".into(),
            platform: "test".into(),
            toolchain: "test".into(),
            build_profile: "release".into(),
            runtime_mode: RuntimeMode::Vm,
            jit_mode: JitMode::InterpreterOnly,
            gc_mode: GcMode::Normal,
            gc_stress_stride: None,
            cache_state: CacheState::NotApplicable,
            execution: ExecutionMetrics {
                wall_time_ns: 1,
                ..ExecutionMetrics::default()
            },
            memory: MemoryMetrics::default(),
            exit_code: Some(0),
            success: true,
            validation: ValidationStatus::Unvalidated,
            validation_marker: None,
            command: vec!["true".into()],
            failure: None,
        };
        assert!(!result.is_scoreable());
    }
}
