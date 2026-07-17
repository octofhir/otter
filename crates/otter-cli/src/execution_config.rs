//! Owned runtime execution configuration for CLI commands.
//!
//! # Contents
//! - [`CliJitTier`] — stable CLI spelling for execution-tier selection.
//! - [`CliExecutionConfig`] — timeout, trace, and JIT settings captured once
//!   after argument parsing and applied to either public runtime builder.
//!
//! # Invariants
//! - Runtime-backed command paths receive this value explicitly. No timeout,
//!   trace, or tier setting travels through process-global mutable state.
//! - Legacy environment variables are translated once at the CLI boundary;
//!   runtime, VM, and JIT crates consume only structured configuration.
//! - `None` keeps the runtime timeout default while `Some(Duration::ZERO)`
//!   explicitly disables it.

use std::io::{self, BufWriter};
use std::time::Duration;

use clap::ValueEnum;
use otter_runtime::{JitSelection, OtterBuilder, RuntimeBuilder, TracerFactory};

/// User-facing execution-tier selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum CliJitTier {
    /// Production optimizing tier with template fallback.
    ProductionTiered,
    /// Template compiler only.
    Template,
    /// Bytecode interpreter only.
    Interpreter,
}

impl From<CliJitTier> for JitSelection {
    fn from(value: CliJitTier) -> Self {
        match value {
            CliJitTier::ProductionTiered => Self::ProductionTiered,
            CliJitTier::Template => Self::Template,
            CliJitTier::Interpreter => Self::InterpreterOnly,
        }
    }
}

/// Owned execution settings shared by every runtime-backed CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliExecutionConfig {
    timeout: Option<Duration>,
    trace_target: Option<String>,
    jit_selection: JitSelection,
    jit_osr_threshold: Option<u32>,
}

impl Default for CliExecutionConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            trace_target: None,
            jit_selection: JitSelection::ProductionTiered,
            jit_osr_threshold: None,
        }
    }
}

impl CliExecutionConfig {
    /// Capture CLI arguments and legacy compatibility knobs exactly once.
    pub(crate) fn new(
        timeout_secs: Option<u64>,
        trace_target: Option<String>,
        jit_tier: Option<CliJitTier>,
    ) -> Self {
        let jit_selection = jit_tier
            .map(JitSelection::from)
            .unwrap_or_else(legacy_jit_selection);
        Self {
            timeout: timeout_secs.map(Duration::from_secs),
            trace_target,
            jit_selection,
            jit_osr_threshold: legacy_jit_osr_threshold(),
        }
    }

    /// Apply execution settings to the async-capable public runtime facade.
    pub(crate) fn apply_otter_builder(&self, builder: OtterBuilder) -> OtterBuilder {
        let mut builder = builder.jit_selection(self.jit_selection);
        if let Some(threshold) = self.jit_osr_threshold {
            builder = builder.jit_osr_threshold(threshold);
        }
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(target) = &self.trace_target {
            builder = builder.tracer_factory(Some(trace_factory_for_target(target)));
        }
        builder
    }

    /// Apply the same execution settings to the local synchronous runtime.
    ///
    /// The runtime currently exposes its timeout as informational on this
    /// direct path; keeping the configured value here avoids another CLI-only
    /// source of truth when direct interruption support lands.
    pub(crate) fn apply_runtime_builder(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        let mut builder = builder.jit_selection(self.jit_selection);
        if let Some(threshold) = self.jit_osr_threshold {
            builder = builder.jit_osr_threshold(threshold);
        }
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(target) = &self.trace_target {
            builder = builder.tracer_factory(Some(trace_factory_for_target(target)));
        }
        builder
    }

    /// Stable CLI spelling for diagnostics and reproducibility metadata.
    pub(crate) const fn jit_tier_name(&self) -> &'static str {
        match self.jit_selection {
            JitSelection::ProductionTiered => "production-tiered",
            JitSelection::Template => "template",
            JitSelection::InterpreterOnly => "interpreter",
        }
    }

    pub(crate) const fn interpreter_only(&self) -> bool {
        matches!(self.jit_selection, JitSelection::InterpreterOnly)
    }
}

fn legacy_jit_selection() -> JitSelection {
    if std::env::var("OTTER_JIT").as_deref() == Ok("0") {
        JitSelection::InterpreterOnly
    } else {
        JitSelection::ProductionTiered
    }
}

fn legacy_jit_osr_threshold() -> Option<u32> {
    std::env::var("OTTER_JIT_OSR_THRESHOLD")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|threshold| *threshold > 0)
}

/// Build a fresh writer per runtime isolate. `-` writes to stderr; any other
/// target is truncated when that isolate constructs its tracer.
fn trace_factory_for_target(target: &str) -> TracerFactory {
    let target = target.to_string();
    TracerFactory::new(move || -> Box<dyn otter_runtime::inspect::StepTracer> {
        let writer: Box<dyn io::Write> = if target == "-" {
            Box::new(BufWriter::new(io::stderr()))
        } else {
            match std::fs::File::create(&target) {
                Ok(file) => Box::new(BufWriter::new(file)),
                Err(err) => {
                    eprintln!(
                        "warning: --trace cannot open {target}: {err}; falling back to stderr"
                    );
                    Box::new(BufWriter::new(io::stderr()))
                }
            }
        };
        Box::new(otter_runtime::inspect::WriterTracer::new(writer))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_tiers_map_without_ambient_state() {
        assert_eq!(
            JitSelection::from(CliJitTier::ProductionTiered),
            JitSelection::ProductionTiered
        );
        assert_eq!(
            JitSelection::from(CliJitTier::Template),
            JitSelection::Template
        );
        assert_eq!(
            JitSelection::from(CliJitTier::Interpreter),
            JitSelection::InterpreterOnly
        );
    }

    #[test]
    fn absent_and_zero_timeout_remain_distinct() {
        let inherited = CliExecutionConfig {
            timeout: None,
            trace_target: None,
            jit_selection: JitSelection::InterpreterOnly,
            jit_osr_threshold: None,
        };
        let disabled = CliExecutionConfig {
            timeout: Some(Duration::ZERO),
            ..inherited.clone()
        };
        assert_ne!(inherited, disabled);
    }

    #[test]
    fn trace_target_is_owned() {
        let mut target = String::from("trace.log");
        let config = CliExecutionConfig {
            timeout: None,
            trace_target: Some(target.clone()),
            jit_selection: JitSelection::InterpreterOnly,
            jit_osr_threshold: None,
        };
        target.clear();
        assert_eq!(config.trace_target.as_deref(), Some("trace.log"));
    }
}
