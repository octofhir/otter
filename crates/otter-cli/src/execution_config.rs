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
//! - Normal CLI execution uses the production tier policy; `--jitless` selects
//!   the template baseline compiler (interpreter plus the template tier, no
//!   optimizing compilation) — the jitless engine, analogous to running with a
//!   bytecode baseline instead of an optimizing JIT; `--interpreter` selects
//!   the bytecode interpreter alone, the differential-testing oracle.
//! - `None` keeps the runtime timeout default while `Some(Duration::ZERO)`
//!   explicitly disables it.
//! - Engine crates only return owned JIT reports. This outer configuration
//!   owns their optional filesystem/stderr serialization.

use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use otter_runtime::{
    JitArtifactBatch, JitDebugReport, JitDebugRequest, JitDebugTier, JitSelection, OtterBuilder,
    TracerFactory, WarningOptions,
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
    warning_options: WarningOptions,
    process_title: Option<String>,
    expose_gc: bool,
    expose_internals: bool,
    /// How much stack a call may use, in kilobytes, as `--stack-size` named it.
    stack_size: Option<u32>,
    flag_spellings: Vec<String>,
    node_options: Vec<String>,
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
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
        }
    }
}

impl CliExecutionConfig {
    /// Capture CLI arguments and the internal OSR diagnostic knob exactly once.
    pub(crate) fn new(
        timeout_secs: Option<u64>,
        trace_target: Option<String>,
        jit_selection: JitSelection,
        jit_events_target: Option<String>,
        jit_artifacts_target: Option<String>,
    ) -> Self {
        Self {
            timeout: timeout_secs.map(Duration::from_secs),
            trace_target,
            jit_events_target,
            jit_artifacts_target,
            jit_selection,
            jit_osr_threshold: legacy_jit_osr_threshold(),
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
        }
    }

    /// Apply execution settings to the async-capable public runtime facade.
    /// Replace the warning-channel switches captured from the CLI flags.
    pub(crate) fn set_warning_options(&mut self, options: WarningOptions) {
        self.warning_options = options;
    }

    /// Set the initial `process.title` captured from `--title`.
    pub(crate) fn set_process_title(&mut self, title: Option<String>) {
        self.process_title = title;
    }

    /// Enable the global `gc()` captured from `--expose-gc`.
    pub(crate) fn set_expose_gc(&mut self, expose: bool) {
        self.expose_gc = expose;
    }

    /// How much stack a call may use, as `--stack-size` named it.
    pub(crate) fn set_stack_size(&mut self, kilobytes: Option<u32>) {
        self.stack_size = kilobytes;
    }

    /// The call depth a stack of the asked-for size holds.
    ///
    /// The switch is written in kilobytes because that is what the platform it
    /// comes from counts; this engine counts frames, so the size is read as
    /// the share of the default stack the run asked for and the depth follows
    /// it. A run asking for a quarter of the usual stack overflows about four
    /// times as soon, which is what asking for it means.
    fn stack_depth(&self) -> Option<u32> {
        const NODE_DEFAULT_STACK_KB: u64 = 984;
        let kilobytes = u64::from(self.stack_size?);
        let frames =
            u64::from(otter_runtime::DEFAULT_MAX_STACK_DEPTH) * kilobytes / NODE_DEFAULT_STACK_KB;
        Some(u32::try_from(frames.max(1)).unwrap_or(u32::MAX))
    }

    /// Record `--expose-internals`. The engine's `internal/*` modules are
    /// always requirable; the flag is what a flag-checking harness reads
    /// back out of `process.execArgv`.
    pub(crate) fn set_expose_internals(&mut self, expose: bool) {
        self.expose_internals = expose;
    }

    /// The process's own argv, so `execArgv` can echo a switch back in the
    /// spelling it arrived in rather than the canonical one.
    pub(crate) fn set_flag_spellings(&mut self, argv: Vec<String>) {
        self.flag_spellings = argv;
    }

    /// Node-style option switches carried into `process.execArgv`, which is
    /// where `internal/options` reads them.
    pub(crate) fn set_node_options(&mut self, switches: Vec<String>) {
        self.node_options = switches;
    }

    /// `canonical` unless the caller wrote a recognized alias of it.
    fn spelled(&self, canonical: &str) -> String {
        let alias = format!("--{}", canonical.trim_start_matches('-').replace('-', "_"));
        if self.flag_spellings.iter().any(|arg| arg == &alias) {
            return alias;
        }
        canonical.to_string()
    }

    /// The node-style engine flags this process is running with, in the
    /// spelling `process.execArgv` reports so a flag-checking harness or a
    /// `spawn(process.execPath, [...process.execArgv, ...])` re-exec
    /// reproduces the configuration.
    fn exec_argv(&self) -> Vec<String> {
        let w = &self.warning_options;
        let mut argv = Vec::new();
        if self.expose_gc {
            argv.push(self.spelled("--expose-gc"));
        }
        if self.expose_internals {
            argv.push(self.spelled("--expose-internals"));
        }
        if let Some(kilobytes) = self.stack_size {
            argv.push(format!("--stack-size={kilobytes}"));
        }
        argv.extend(self.node_options.iter().cloned());
        if w.no_warnings {
            argv.push("--no-warnings".to_string());
        }
        if w.no_deprecation {
            argv.push("--no-deprecation".to_string());
        }
        if w.throw_deprecation {
            argv.push("--throw-deprecation".to_string());
        }
        if w.trace_warnings {
            argv.push("--trace-warnings".to_string());
        }
        if w.pending_deprecation {
            argv.push("--pending-deprecation".to_string());
        }
        for code in &w.disabled {
            argv.push(format!("--disable-warning={code}"));
        }
        if let Some(title) = &self.process_title {
            argv.push(format!("--title={title}"));
        }
        argv
    }

    pub(crate) fn apply_otter_builder(&self, builder: OtterBuilder) -> OtterBuilder {
        let mut builder = builder
            .jit_selection(self.jit_selection)
            .jit_debug(self.jit_debug_request())
            .warning_options(self.warning_options.clone())
            .process_title(self.process_title.clone())
            .process_exec_argv(self.exec_argv())
            // Node lets the main thread park in `Atomics.wait`; the CLI owns a
            // watchdog and an interrupt handle, so the park is cancellable.
            .allow_blocking_atomics_wait(true)
            .expose_gc(self.expose_gc);
        if let Some(depth) = self.stack_depth() {
            builder = builder.max_stack_depth(depth);
        }
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
            JitSelection::ProductionTiered => "production",
            JitSelection::Template => "jitless",
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
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
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
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
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
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
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
            warning_options: WarningOptions::default(),
            process_title: None,
            expose_gc: false,
            expose_internals: false,
            stack_size: None,
            flag_spellings: Vec::new(),
            node_options: Vec::new(),
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
