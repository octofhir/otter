//! Cold direct-compiler measurement and validation boundary.
//!
//! # Contents
//! - [`JitMeasurementTier`] — explicit compiler tier for one sample.
//! - [`JitCompilerProbe`] — opaque production compiler installation with
//!   scalar compile-time counters.
//! - [`MeasuredCompilation`] — opaque ownership of one measured code object.
//! - [`MeasuredValidation`] — JIT-owned exact-generation validation hook with
//!   a scalar successful-return counter.
//!
//! # Invariants
//! - Benchmark clients never receive or implement [`otter_vm::JitFunctionCode`]
//!   and never own runtime-stub, native-frame, safepoint, dependency, or result
//!   ABI carriers.
//! - Validation republishes the exact measured generation and delegates every
//!   code-object capability unchanged. Only successful entry outcomes update
//!   the owned scalar counter.
//! - This API is cold benchmark instrumentation and is never installed by the
//!   production tier policy.
//!
//! # See also
//! - [`crate::OtterJitCompiler`] — production compiler implementation.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use otter_vm::{
    Interpreter, JitArtifactBundle, JitCompileError, JitCompileRequest, JitCompileStatus,
    JitCompilerHook, JitExecOutcome, JitFunctionCode, JitRuntimeStubBinding, VmRuntimeActivation,
};

use crate::OtterJitCompiler;

/// Owned scalar compiler measurements for one runtime session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitCompilerMeasurement {
    /// Compiler hook invocations.
    pub invocations: u64,
    /// Sum of compiler wall time in nanoseconds.
    pub wall_time_ns: u64,
    /// Longest compiler invocation in nanoseconds.
    pub max_wall_time_ns: u64,
    /// Successfully emitted code objects.
    pub emitted_code_objects: u64,
    /// Sum of finalized native code bytes.
    pub emitted_code_bytes: u64,
}

/// JIT-owned production compiler probe.
///
/// Benchmark clients can install the compiler and read scalar counters, but
/// never receive its VM hook, runtime-stub bindings, or compiled-code objects.
pub struct JitCompilerProbe {
    hook: Arc<dyn JitCompilerHook>,
    stats: Arc<Mutex<JitCompilerMeasurement>>,
}

impl JitCompilerProbe {
    /// Wrap one production compiler with cold measurement counters.
    #[must_use]
    pub fn new(compiler: Arc<OtterJitCompiler>) -> Self {
        let stats = Arc::new(Mutex::new(JitCompilerMeasurement::default()));
        let hook: Arc<dyn JitCompilerHook> = Arc::new(MeasuredCompilerHook {
            inner: compiler,
            stats: Arc::clone(&stats),
        });
        Self { hook, stats }
    }

    /// Install the measured compiler without exposing its VM hook.
    pub fn install(&self, interpreter: &mut Interpreter) {
        interpreter.set_jit_compiler(Some(Arc::clone(&self.hook)));
    }

    /// Snapshot the current scalar compiler counters.
    #[must_use]
    pub fn measurement(&self) -> JitCompilerMeasurement {
        *self.stats.lock().expect("JIT compiler probe lock poisoned")
    }
}

struct MeasuredCompilerHook {
    inner: Arc<OtterJitCompiler>,
    stats: Arc<Mutex<JitCompilerMeasurement>>,
}

impl MeasuredCompilerHook {
    fn record(&self, elapsed_ns: u64, result: &Result<JitCompileStatus, JitCompileError>) {
        let mut stats = self.stats.lock().expect("JIT compiler probe lock poisoned");
        stats.invocations = stats.invocations.saturating_add(1);
        stats.wall_time_ns = stats.wall_time_ns.saturating_add(elapsed_ns);
        stats.max_wall_time_ns = stats.max_wall_time_ns.max(elapsed_ns);
        if let Ok(JitCompileStatus::Compiled { code, .. }) = result {
            stats.emitted_code_objects = stats.emitted_code_objects.saturating_add(1);
            stats.emitted_code_bytes = stats
                .emitted_code_bytes
                .saturating_add(u64::try_from(code.code_len()).unwrap_or(u64::MAX));
        }
    }
}

impl JitCompilerHook for MeasuredCompilerHook {
    fn optimizing_tier_enabled(&self) -> bool {
        self.inner.optimizing_tier_enabled()
    }

    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        self.inner.runtime_stub_bindings()
    }

    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        let started = std::time::Instant::now();
        let result = self.inner.compile_function(request);
        self.record(elapsed_ns(started), &result);
        result
    }

    fn compile_optimized_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        let started = std::time::Instant::now();
        let result = self.inner.compile_optimized_function(request);
        self.record(elapsed_ns(started), &result);
        result
    }
}

fn elapsed_ns(started: std::time::Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Native tier compiled by one measurement sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitMeasurementTier {
    /// Template baseline compiler.
    Template,
    /// Machine IR optimizing compiler.
    Optimizing,
}

impl JitMeasurementTier {
    fn name(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Optimizing => "optimizing",
        }
    }
}

/// Opaque ownership of one directly measured native compilation.
///
/// Clients may inspect only scalar code size, entry eligibility, and the owned
/// artifact bundle. Executable code and the VM↔JIT ABI stay inside this crate.
pub struct MeasuredCompilation {
    tier: JitMeasurementTier,
    compiler: Arc<OtterJitCompiler>,
    code: Arc<dyn JitFunctionCode>,
    artifact: Option<Box<JitArtifactBundle>>,
}

impl MeasuredCompilation {
    /// Finalized native code bytes for this object.
    #[must_use]
    pub fn code_len(&self) -> usize {
        self.code.code_len()
    }

    /// Whether this object is restricted to loop-header OSR entry.
    #[must_use]
    pub fn is_osr_only(&self) -> bool {
        self.code.osr_only()
    }

    /// Optional owned diagnostic artifact captured for this compilation.
    #[must_use]
    pub fn artifact(&self) -> Option<&JitArtifactBundle> {
        self.artifact.as_deref()
    }

    /// Convert this exact generation into an installable validation session.
    #[must_use]
    pub fn into_validation(self) -> MeasuredValidation {
        let function_id = self.code.metadata().code_block_id;
        let returned_entries = Arc::new(AtomicU64::new(0));
        let observed: Arc<dyn JitFunctionCode> = Arc::new(ObservedJitCode {
            code: self.code,
            returned_entries: Arc::clone(&returned_entries),
        });
        let hook: Arc<dyn JitCompilerHook> = Arc::new(ExactMeasuredCompiler {
            function_id,
            tier: self.tier,
            code: observed,
            runtime_stub_bindings: self.compiler.runtime_stub_bindings(),
        });
        MeasuredValidation {
            hook,
            returned_entries,
        }
    }
}

/// JIT-owned exact-generation validation session.
///
/// The only observable result is a scalar count. Installation does not expose
/// the compiler hook or any compiled-code carrier to the caller.
pub struct MeasuredValidation {
    hook: Arc<dyn JitCompilerHook>,
    returned_entries: Arc<AtomicU64>,
}

impl MeasuredValidation {
    /// Install this exact measured generation in `interpreter`.
    pub fn install(&self, interpreter: &mut Interpreter) {
        interpreter.set_jit_compiler(Some(Arc::clone(&self.hook)));
    }

    /// Number of compiled entries that returned normally.
    #[must_use]
    pub fn returned_entries(&self) -> u64 {
        self.returned_entries.load(Ordering::Relaxed)
    }
}

impl OtterJitCompiler {
    /// Install this production compiler without exposing its VM hook.
    pub fn install(self: &Arc<Self>, interpreter: &mut Interpreter) {
        let hook: Arc<dyn JitCompilerHook> = self.clone();
        interpreter.set_jit_compiler(Some(hook));
    }

    /// Compile one untimed warmup or timed measurement sample.
    ///
    /// The returned object deliberately keeps executable code and VM↔JIT ABI
    /// records opaque.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic string when the selected compiler is unavailable
    /// or declines the snapshot.
    pub fn compile_measurement(
        self: &Arc<Self>,
        tier: JitMeasurementTier,
        request: JitCompileRequest,
    ) -> Result<MeasuredCompilation, String> {
        let status = match tier {
            JitMeasurementTier::Template => self.compile_function(request),
            JitMeasurementTier::Optimizing => self.compile_optimized_function(request),
        }
        .map_err(|error| error.to_string())?;
        match status {
            JitCompileStatus::Compiled { code, artifact, .. } => Ok(MeasuredCompilation {
                tier,
                compiler: Arc::clone(self),
                code,
                artifact,
            }),
            JitCompileStatus::Unavailable => Err(format!("{} compiler unavailable", tier.name())),
            JitCompileStatus::Unsupported { reason } => {
                Err(format!("{} compiler declined: {reason}", tier.name()))
            }
        }
    }
}

struct ExactMeasuredCompiler {
    function_id: u32,
    tier: JitMeasurementTier,
    code: Arc<dyn JitFunctionCode>,
    runtime_stub_bindings: Vec<JitRuntimeStubBinding>,
}

impl ExactMeasuredCompiler {
    fn exact_status(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        if request.snapshot.code_block.id != self.function_id {
            return Ok(JitCompileStatus::Unsupported {
                reason: "validation hook exposes only the measured function".into(),
            });
        }
        if request.code_object_id != self.code.metadata().id {
            return Err(JitCompileError::new(format!(
                "measured artifact id {} cannot install as {}",
                self.code.metadata().id,
                request.code_object_id
            )));
        }
        Ok(JitCompileStatus::Compiled {
            code: Arc::clone(&self.code),
            artifact: None,
            diagnostics: Box::default(),
            ir_node_count: 0,
        })
    }
}

impl JitCompilerHook for ExactMeasuredCompiler {
    fn optimizing_tier_enabled(&self) -> bool {
        self.tier == JitMeasurementTier::Optimizing
    }

    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        self.runtime_stub_bindings.clone()
    }

    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        if self.tier == JitMeasurementTier::Template {
            self.exact_status(request)
        } else {
            Ok(JitCompileStatus::Unsupported {
                reason: "validation hook reserves the measured artifact for optimizing entry"
                    .into(),
            })
        }
    }

    fn compile_optimized_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        if self.tier == JitMeasurementTier::Optimizing {
            self.exact_status(request)
        } else {
            Ok(JitCompileStatus::Unavailable)
        }
    }
}

#[derive(Debug)]
struct ObservedJitCode {
    code: Arc<dyn JitFunctionCode>,
    returned_entries: Arc<AtomicU64>,
}

impl ObservedJitCode {
    fn note(&self, outcome: Option<&JitExecOutcome>) {
        if matches!(outcome, Some(JitExecOutcome::Returned(_))) {
            self.returned_entries.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl JitFunctionCode for ObservedJitCode {
    fn metadata(&self) -> otter_vm::native_abi::CodeObjectMetadata {
        self.code.metadata()
    }

    fn native_frame_kind(&self) -> otter_vm::native_abi::NativeFrameKind {
        self.code.native_frame_kind()
    }

    fn generated_stack_frame_bytes(&self) -> Option<u32> {
        self.code.generated_stack_frame_bytes()
    }

    fn generated_entry_uses_parameter_prefix(&self) -> bool {
        self.code.generated_entry_uses_parameter_prefix()
    }

    fn dependencies(&self) -> &[otter_vm::native_abi::CodeDependency] {
        self.code.dependencies()
    }

    fn code_len(&self) -> usize {
        self.code.code_len()
    }

    fn osr_only(&self) -> bool {
        self.code.osr_only()
    }

    fn entry_addr(&self) -> Option<usize> {
        self.code.entry_addr()
    }

    fn safepoint_count(&self) -> u32 {
        self.code.safepoint_count()
    }

    fn safepoint_record(
        &self,
        safepoint_id: otter_vm::native_abi::SafepointId,
    ) -> Option<&otter_vm::native_abi::SafepointRecord> {
        self.code.safepoint_record(safepoint_id)
    }

    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome {
        let outcome = self.code.run_entry(activation);
        self.note(Some(&outcome));
        outcome
    }

    fn run_optimized_entry(&self, activation: VmRuntimeActivation) -> Option<JitExecOutcome> {
        let outcome = self.code.run_optimized_entry(activation);
        self.note(outcome.as_ref());
        outcome
    }

    fn run_optimized_osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        let outcome = self.code.run_optimized_osr_entry(activation, logical_pc);
        self.note(outcome.as_ref());
        outcome
    }

    fn osr_entry(
        &self,
        activation: VmRuntimeActivation,
        logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        let outcome = self.code.osr_entry(activation, logical_pc);
        self.note(outcome.as_ref());
        outcome
    }
}
