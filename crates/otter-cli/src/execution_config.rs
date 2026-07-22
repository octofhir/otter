//! Owned runtime execution configuration for CLI commands.
//!
//! # Contents
//! - [`CliExecutionConfig`] — timeout, trace, JIT tier, and structured JIT
//!   diagnostics captured once after argument parsing and applied to either
//!   public runtime builder.
//!
//! # Invariants
//! - Runtime-backed command paths receive this value explicitly. No timeout,
//!   trace, or tier setting travels through process-global mutable state.
//! - Normal CLI execution always uses the production tier policy; `--jitless`
//!   selects the same interpreter-only runtime path used by the semantic oracle.
//! - `None` keeps the runtime timeout default while `Some(Duration::ZERO)`
//!   explicitly disables it.
//! - Engine crates only return owned JIT reports. This outer configuration
//!   owns their optional filesystem/stderr serialization.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use otter_runtime::{
    JitArtifactBatch, JitDebugReport, JitDebugRequest, JitDebugTier, JitSelection, OtterBuilder,
    RuntimeBuilder, TracerFactory,
};

/// Owned execution settings shared by every runtime-backed CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliExecutionConfig {
    timeout: Option<Duration>,
    trace_target: Option<String>,
    jit_events_target: Option<String>,
    jit_artifacts_target: Option<String>,
    jit_selection: JitSelection,
    jit_osr_threshold: Option<u32>,
}

impl Default for CliExecutionConfig {
    fn default() -> Self {
        Self {
            timeout: None,
            trace_target: None,
            jit_events_target: None,
            jit_artifacts_target: None,
            jit_selection: JitSelection::ProductionTiered,
            jit_osr_threshold: None,
        }
    }
}

impl CliExecutionConfig {
    /// Capture CLI arguments and the internal OSR diagnostic knob exactly once.
    pub(crate) fn new(
        timeout_secs: Option<u64>,
        trace_target: Option<String>,
        jitless: bool,
        jit_events_target: Option<String>,
        jit_artifacts_target: Option<String>,
    ) -> Self {
        let jit_selection = if jitless {
            JitSelection::InterpreterOnly
        } else {
            JitSelection::ProductionTiered
        };
        Self {
            timeout: timeout_secs.map(Duration::from_secs),
            trace_target,
            jit_events_target,
            jit_artifacts_target,
            jit_selection,
            jit_osr_threshold: legacy_jit_osr_threshold(),
        }
    }

    /// Apply execution settings to the async-capable public runtime facade.
    pub(crate) fn apply_otter_builder(&self, builder: OtterBuilder) -> OtterBuilder {
        let mut builder = builder
            .jit_selection(self.jit_selection)
            .jit_debug(self.jit_debug_request());
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
        let mut builder = builder
            .jit_selection(self.jit_selection)
            .jit_debug(self.jit_debug_request());
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
    pub(crate) const fn execution_mode_name(&self) -> &'static str {
        match self.jit_selection {
            JitSelection::ProductionTiered | JitSelection::Template => "production",
            JitSelection::InterpreterOnly => "interpreter",
        }
    }

    /// Return whether the CLI must retain and serialize structured JIT events.
    pub(crate) const fn jit_events_enabled(&self) -> bool {
        self.jit_events_target.is_some()
    }

    /// Return whether the CLI must persist successful compile bundles.
    pub(crate) const fn jit_artifacts_enabled(&self) -> bool {
        self.jit_artifacts_target.is_some()
    }

    /// Serialize one complete JIT report to the configured target.
    ///
    /// `-` writes to stderr; a path is created or truncated exactly once by the
    /// outer CLI command after all top-level runs have completed.
    pub(crate) fn write_jit_debug_report(&self, report: &JitDebugReport) -> io::Result<()> {
        let Some(target) = &self.jit_events_target else {
            return Ok(());
        };
        let mut writer: Box<dyn Write> = if target == "-" {
            Box::new(BufWriter::new(io::stderr()))
        } else {
            Box::new(BufWriter::new(std::fs::File::create(target)?))
        };
        serde_json::to_writer_pretty(&mut writer, report).map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// Make one bounded artifact batch atomically visible.
    ///
    /// The final root must not exist. All compile directories are written to a
    /// private sibling first, then the complete root is renamed into place.
    /// This is a cooperative single-writer contract, not crash-durable storage
    /// or a cross-process no-clobber primitive.
    pub(crate) fn write_jit_artifacts(&self, batch: &JitArtifactBatch) -> io::Result<()> {
        let Some(target) = &self.jit_artifacts_target else {
            return Ok(());
        };
        if target == "-" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--jit-artifacts requires a directory path",
            ));
        }
        let target = PathBuf::from(target);
        if target.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("JIT artifact target already exists: {}", target.display()),
            ));
        }
        let parent = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let file_name = target.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "JIT artifact target must name a directory",
            )
        })?;
        let temp = parent.join(format!(
            ".{}.tmp-{}",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        if temp.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "JIT artifact temporary target already exists: {}",
                    temp.display()
                ),
            ));
        }
        std::fs::create_dir(&temp)?;

        let write_result = (|| {
            let mut directory_names = Vec::with_capacity(batch.bundles().len());
            for (ordinal, bundle) in batch.bundles().iter().enumerate() {
                let manifest = bundle.manifest();
                let tier = match manifest.tier() {
                    JitDebugTier::Template => "template",
                    JitDebugTier::Optimizing => "optimizing",
                };
                let directory_name = format!(
                    "jit-{ordinal:04}-{tier}-f{}-c{}",
                    manifest.function_id(),
                    manifest.code_object_id()
                );
                let directory = temp.join(&directory_name);
                std::fs::create_dir(&directory)?;
                write_json_file(&directory.join("manifest.json"), manifest)?;
                for file in bundle.files() {
                    let path = directory.join(file.name().as_str());
                    let mut writer = BufWriter::new(std::fs::File::create(path)?);
                    writer.write_all(file.contents())?;
                    writer.flush()?;
                }
                directory_names.push(directory_name);
            }

            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Index<'a> {
                bundles: &'a [String],
                retained_bytes: u64,
                dropped_bundles: u64,
                dropped_bytes: u64,
                truncated: bool,
            }

            write_json_file(
                &temp.join("index.json"),
                &Index {
                    bundles: &directory_names,
                    retained_bytes: u64::try_from(batch.retained_bytes()).unwrap_or(u64::MAX),
                    dropped_bundles: batch.dropped_bundles(),
                    dropped_bytes: batch.dropped_bytes(),
                    truncated: batch.truncated(),
                },
            )?;
            if target.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "JIT artifact target appeared while writing: {}",
                        target.display()
                    ),
                ));
            }
            std::fs::rename(&temp, &target)
        })();

        if write_result.is_err() {
            let _ = std::fs::remove_dir_all(&temp);
        }
        write_result
    }

    const fn jit_debug_request(&self) -> JitDebugRequest {
        JitDebugRequest::disabled()
            .with_events(self.jit_events_target.is_some())
            .with_artifacts(self.jit_artifacts_target.is_some())
    }
}

fn write_json_file(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let mut writer = BufWriter::new(std::fs::File::create(path)?);
    serde_json::to_writer_pretty(&mut writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
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
    fn absent_and_zero_timeout_remain_distinct() {
        let inherited = CliExecutionConfig {
            timeout: None,
            trace_target: None,
            jit_events_target: None,
            jit_artifacts_target: None,
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
            jit_events_target: None,
            jit_artifacts_target: None,
            jit_selection: JitSelection::InterpreterOnly,
            jit_osr_threshold: None,
        };
        target.clear();
        assert_eq!(config.trace_target.as_deref(), Some("trace.log"));
    }

    #[test]
    fn jit_events_are_default_off_and_owned_when_enabled() {
        assert!(!CliExecutionConfig::default().jit_events_enabled());
        assert!(!CliExecutionConfig::default().jit_artifacts_enabled());
        assert_eq!(
            CliExecutionConfig::default().jit_debug_request(),
            JitDebugRequest::disabled()
        );

        let mut target = String::from("jit-events.json");
        let config = CliExecutionConfig {
            timeout: None,
            trace_target: None,
            jit_events_target: Some(target.clone()),
            jit_artifacts_target: None,
            jit_selection: JitSelection::Template,
            jit_osr_threshold: Some(1),
        };
        target.clear();
        assert!(config.jit_events_enabled());
        assert_eq!(config.jit_events_target.as_deref(), Some("jit-events.json"));
        assert_eq!(config.jit_debug_request(), JitDebugRequest::events());

        let artifacts = CliExecutionConfig {
            timeout: None,
            trace_target: None,
            jit_events_target: None,
            jit_artifacts_target: Some("jit-artifacts".to_string()),
            jit_selection: JitSelection::Template,
            jit_osr_threshold: Some(1),
        };
        assert!(artifacts.jit_artifacts_enabled());
        assert!(!artifacts.jit_events_enabled());
        assert_eq!(artifacts.jit_debug_request(), JitDebugRequest::artifacts());

        let both = CliExecutionConfig {
            jit_events_target: Some("jit-events.json".to_string()),
            ..artifacts
        };
        assert_eq!(
            both.jit_debug_request(),
            JitDebugRequest::disabled()
                .with_events(true)
                .with_artifacts(true)
        );
    }
}
