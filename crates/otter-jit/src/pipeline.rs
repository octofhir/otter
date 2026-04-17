//! JIT compilation pipeline: bytecode → template baseline → machine code.
//!
//! The pipeline owns a single `Tier::Baseline` path: the template
//! baseline lowers a narrow int32-arithmetic-loop subset of the Ignition
//! bytecode into a host-native baseline stencil. Anything outside that
//! subset is rejected (returned as `JitError::UnsupportedInstruction`
//! from the analyzer), and the caller (tier-up hook) falls back to the
//! interpreter.
//!
//! Earlier iterations of OtterJS also carried a MIR → CLIF Tier 2
//! pipeline and a v1 legacy baseline. Both have been retired; only the
//! v2 template baseline ships.

use std::collections::HashMap;
use std::time::Instant;

use otter_vm as vm;
use otter_vm::feedback::FeedbackVector;

use crate::baseline::{analyze_template_candidate, emit_template_stencil};
use crate::code_memory::{CompiledCodeOrigin, CompiledFunction, compile_code_buffer};
use crate::telemetry;
use crate::{BailoutReason, JitError};

/// Result of attempting JIT execution.
///
/// Returned by [`crate::tier_up_hook::DefaultTierUpHook::execute_cached`]
/// when a compiled function finishes (or bails out). `NotCompiled` is a
/// signal to the interpreter to keep stepping bytecode rather than
/// transferring control into the native stencil.
#[derive(Debug)]
pub enum JitExecResult {
    /// Compiled code ran to completion; `u64` is the NaN-boxed return value.
    Ok(u64),
    /// Compiled code bailed out; resume the interpreter at `bytecode_pc`.
    Bailout {
        bytecode_pc: u32,
        reason: BailoutReason,
    },
    /// Function is ineligible or has no cached stencil.
    NotCompiled,
}

// ============================================================
// Helper symbol registration
// ============================================================
//
// JIT code currently calls zero runtime helpers, but the registry is
// retained so that future bailout / write-barrier / allocation-slow-path
// helpers can be installed from the runtime side without rewiring the
// pipeline.

thread_local! {
    /// Helper address lookup: name → address.
    static HELPER_ADDRS: std::cell::RefCell<HashMap<&'static str, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Register runtime helper symbols for JIT compilation.
///
/// Any symbol registered here becomes resolvable via
/// [`lookup_helper_address`] until [`clear_helper_symbols`] is called.
pub fn register_helper_symbols(symbols: Vec<(&'static str, *const u8)>) {
    HELPER_ADDRS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for &(name, ptr) in &symbols {
            map.insert(name, ptr as usize);
        }
    });
}

/// Look up a helper function address by symbol name.
#[must_use]
pub fn lookup_helper_address(name: &str) -> Option<usize> {
    HELPER_ADDRS.with(|m| m.borrow().get(name).copied())
}

/// Clear the helper symbol table. Called on runtime teardown so that
/// helper pointers from a destroyed runtime are not reused by a later one.
pub fn clear_helper_symbols() {
    HELPER_ADDRS.with(|m| m.borrow_mut().clear());
}

// ============================================================
// Compilation API
// ============================================================

/// Attempt to compile `function` with the template baseline.
///
/// Returns `Err(JitError::UnsupportedInstruction(_))` when the function
/// does not fit the baseline subset; the tier-up hook treats that as a
/// soft "leave running in the interpreter" signal rather than a crash.
/// On supported hosts (`aarch64`, `x86_64`) a successful compile returns a [`CompiledFunction`]
/// whose `entry` pointer is callable as
/// `extern "C" fn(*mut JitContext) -> u64`. Other hosts
/// return `Err(JitError::UnsupportedHostArch(_))`.
pub fn compile_function(function: &vm::Function) -> Result<CompiledFunction, JitError> {
    compile_template_baseline(function, None)
}

/// Feedback-aware variant of [`compile_function`]. Currently identical in
/// behaviour because the baseline stencil does not yet read from the
/// persistent feedback vector — the arg is accepted so callers don't need
/// to branch on `Option<FeedbackVector>`, and future speculative variants
/// (int32-trust, shape IC promotion) can be gated on it without
/// breaking the call site.
pub fn compile_function_with_feedback(
    function: &vm::Function,
    feedback: &FeedbackVector,
) -> Result<CompiledFunction, JitError> {
    compile_template_baseline(function, Some(feedback))
}

fn compile_template_baseline(
    function: &vm::Function,
    _feedback: Option<&FeedbackVector>,
) -> Result<CompiledFunction, JitError> {
    let started = Instant::now();

    let program = analyze_template_candidate(function)
        .map_err(|err| JitError::UnsupportedInstruction(format!("{err:?}")))?;

    let buffer = emit_template_stencil(&program)
        .map_err(|err| JitError::UnsupportedInstruction(format!("{err:?}")))?;

    let compiled = compile_code_buffer(&buffer, CompiledCodeOrigin::TemplateBaseline)
        .map_err(|err| JitError::Internal(format!("{err:?}")))?;

    let elapsed_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    telemetry::record_compile_time(true, elapsed_ns);
    let name = function.name().unwrap_or("<anonymous>");
    telemetry::record_function_compiled(name, 1, elapsed_ns, compiled.size());

    Ok(compiled)
}
