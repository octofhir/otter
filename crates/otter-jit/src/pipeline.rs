//! JIT compilation pipeline: bytecode → MIR → CLIF → machine code.
//!
//! This is the main entry point for compiling and executing JIT code.

use std::collections::HashMap;
use std::time::Instant;

use otter_vm as vm;

use crate::baseline::{analyze_template_candidate, emit_template_stencil};
#[cfg(feature = "bytecode_v2")]
use crate::baseline::v2::{analyze_v2_template_candidate, emit_v2_template_stencil};
use crate::code_memory::{
    CompiledCodeOrigin, CompiledFunction, compile_clif_function, compile_code_buffer,
    create_host_isa,
};
use crate::codegen::lower::lower_mir_to_clif;
use crate::context::JitContext;
use crate::mir::builder::build_mir;
use crate::mir::verify::verify;
use crate::telemetry;
use crate::{BAILOUT_SENTINEL, JitError};

/// Result of attempting JIT execution.
#[derive(Debug)]
pub enum JitExecResult {
    /// JIT code ran successfully, return value is NaN-boxed u64.
    Ok(u64),
    /// JIT code bailed out — resume interpreter at this bytecode PC.
    Bailout {
        bytecode_pc: u32,
        reason: crate::BailoutReason,
    },
    /// Function not eligible or compilation failed.
    NotCompiled,
}

/// Strategy selected for Tier 1 compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier1Strategy {
    /// The function matches the narrow `bytecode -> asm stencil` subset.
    TemplateBaseline,
    /// Fall back to the existing `bytecode -> MIR -> CLIF` baseline path.
    MirBaseline,
}

// ============================================================
// Helper symbol registration
// ============================================================

thread_local! {
    /// Helper address lookup: name → address. O(1).
    static HELPER_ADDRS: std::cell::RefCell<HashMap<&'static str, usize>> = std::cell::RefCell::new(HashMap::new());
}

/// Register runtime helper symbols for JIT compilation.
pub fn register_helper_symbols(symbols: Vec<(&'static str, *const u8)>) {
    HELPER_ADDRS.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for &(name, ptr) in &symbols {
            map.insert(name, ptr as usize);
        }
    });
}

/// Look up a helper function address by symbol name. O(1).
pub fn lookup_helper_address(name: &str) -> Option<usize> {
    HELPER_ADDRS.with(|m| m.borrow().get(name).copied())
}

/// Clear the helper symbol table (called on runtime teardown).
pub fn clear_helper_symbols() {
    HELPER_ADDRS.with(|m| m.borrow_mut().clear());
}

// ============================================================
// Compilation & Execution API
// ============================================================

/// Compile a VM function into machine code for the Tier 1 subset.
pub fn compile_function(function: &vm::Function) -> Result<CompiledFunction, JitError> {
    compile_function_profiled(function, &[])
}

/// Try to lower the function through the v2 (Ignition-style) template
/// baseline. Returns `Some(compiled)` if the function carries v2
/// bytecode and the analyzer + emitter both accept it; `None` to fall
/// through to the v1 baseline / MIR pipelines.
///
/// Feature-gated: returns `None` when the `bytecode_v2` feature is off.
#[cfg(feature = "bytecode_v2")]
fn try_compile_v2_template(function: &vm::Function) -> Option<CompiledFunction> {
    if function.bytecode_v2().is_none() {
        return None;
    }
    let program = analyze_v2_template_candidate(function).ok()?;
    let stencil = emit_v2_template_stencil(&program).ok()?;
    let cfg = crate::config::jit_config();
    if cfg.dump_asm {
        eprintln!(
            "[JIT] === v2 Template Baseline Stencil for {:?} ({} bytes) ===",
            function.name(),
            stencil.len(),
        );
        crate::codegen::disasm::dump_disassembly(
            stencil.bytes(),
            0,
            Some("v2-template-baseline-stencil"),
        );
    }
    compile_code_buffer(&stencil, CompiledCodeOrigin::TemplateBaseline).ok()
}

#[cfg(not(feature = "bytecode_v2"))]
#[inline]
fn try_compile_v2_template(_function: &vm::Function) -> Option<CompiledFunction> {
    None
}

/// Choose the preferred Tier 1 strategy for a function.
///
/// Today this is an analysis/planning decision. Even when the template baseline
/// path is selected, executable installation still falls back to the existing
/// MIR/CLIF backend until raw stencil installation is wired into the runtime.
#[must_use]
pub fn select_tier1_strategy(
    function: &vm::Function,
    property_profile: &[Option<vm::PropertyInlineCache>],
) -> Tier1Strategy {
    match analyze_template_candidate(function, property_profile)
        .ok()
        .and_then(|program| emit_template_stencil(&program).ok().map(|_| program))
    {
        Some(_) => Tier1Strategy::TemplateBaseline,
        None => Tier1Strategy::MirBaseline,
    }
}

/// Compile with persistent FeedbackVector.
///
/// If feedback shows stable Int32 arithmetic, the compiler uses speculative
/// typed operations instead of generic helpers. If feedback shows monomorphic
/// property access, the compiler emits shape-guarded fast paths.
///
/// This is the primary production entry point — called by the runtime when
/// feedback is available from previous interpreter runs.
pub fn compile_function_with_feedback(
    function: &vm::Function,
    feedback: &vm::feedback::FeedbackVector,
) -> Result<CompiledFunction, JitError> {
    use vm::feedback::{FeedbackSlotData, FeedbackSlotId};

    let function_name = function.name().unwrap_or("<anonymous>");
    let cfg = crate::config::jit_config();
    let start = Instant::now();

    // v2 (Ignition-style accumulator) baseline probe. When the function
    // carries v2 bytecode and the Phase 4.1/4.2 analyzer + emitter accept
    // it, install the x21-pinned stencil directly — a 3× size reduction
    // from the v1 template baseline. Feature-gated so non-feature builds
    // pay zero cost.
    if let Some(compiled) = try_compile_v2_template(function) {
        let duration_ns = start.elapsed().as_nanos() as u64;
        telemetry::record_compile_time(true, duration_ns);
        telemetry::record_function_compiled(function_name, 1, duration_ns, compiled.code_size);
        if cfg.dump_bytecode {
            eprintln!(
                "[JIT] v2 template baseline for {:?}: {:?}",
                function.name(),
                compiled.origin,
            );
        }
        return Ok(compiled);
    }

    // Extract legacy PropertyInlineCache profile from the feedback vector
    // for the MIR path (shape-guarded lowering).
    let property_profile = extract_property_profile(function, feedback);

    // Direct `bytecode → asm template baseline` fast path. JSC-Baseline
    // model: when the analyzer accepts the function and emission succeeds,
    // we install the dense int32-tag-guarded stencil directly; the MIR+CLIF
    // path below is the fallback for opcodes the template doesn't yet cover
    // (float64 arithmetic, generic property access, calls beyond
    // `CallDirect`, etc.). We thread the persistent feedback in here so the
    // analyzer can mark arithmetic ops with stable `Int32` feedback as
    // guard-free, saving ~12 asm instructions per operand load.
    if let Ok(program) =
        crate::baseline::analyze_template_candidate_with_feedback(
            function,
            &property_profile,
            Some(feedback),
        )
        && let Ok(stencil) = emit_template_stencil(&program)
    {
        if cfg.dump_asm {
            eprintln!(
                "[JIT] === Template Baseline Stencil (feedback) for {:?} ({} bytes) ===",
                function.name(),
                stencil.len(),
            );
            crate::codegen::disasm::dump_disassembly(
                stencil.bytes(),
                0,
                Some("template-baseline-stencil-feedback"),
            );
        }
        if let Ok(compiled) =
            compile_code_buffer(&stencil, CompiledCodeOrigin::TemplateBaseline)
        {
            let duration_ns = start.elapsed().as_nanos() as u64;
            telemetry::record_compile_time(true, duration_ns);
            telemetry::record_function_compiled(function_name, 1, duration_ns, compiled.code_size);
            if cfg.dump_bytecode {
                eprintln!(
                    "[JIT] tier1 (feedback-aware) backend for {:?}: {:?}",
                    function.name(),
                    compiled.origin,
                );
            }
            return Ok(compiled);
        }
    }

    // Determine compilation tier based on feedback quality.
    let has_useful_feedback = (0..feedback.len()).any(|i| {
        let id = FeedbackSlotId(i as u16);
        matches!(
            feedback.get(id),
            Some(FeedbackSlotData::Arithmetic(fb)) if *fb != vm::feedback::ArithmeticFeedback::None
        ) || matches!(
            feedback.get(id),
            Some(FeedbackSlotData::Property(fb)) if fb.as_monomorphic().is_some()
        )
    });

    let tier = if has_useful_feedback {
        crate::mir::passes::PassTier::Optimized
    } else {
        crate::mir::passes::PassTier::Baseline
    };

    if cfg.dump_bytecode {
        eprintln!(
            "[JIT] === Bytecode for {:?} ({} instructions, tier={:?}) ===",
            function.name(),
            function.bytecode().len(),
            tier
        );
        for (pc, instr) in function.bytecode().instructions().iter().enumerate() {
            eprintln!(
                "  {:04}: {:?} a={} b={} c={}",
                pc,
                instr.opcode(),
                instr.a(),
                instr.b(),
                instr.c()
            );
        }
    }

    // Build MIR using existing builder (property profile reused from the
    // template-baseline probe above).
    let mut graph = build_mir(
        function,
        if property_profile.is_empty() {
            None
        } else {
            Some(&property_profile)
        },
    )?;

    if cfg.dump_mir {
        eprintln!(
            "[JIT] === MIR (before passes, feedback-aware) for {:?} ===",
            function.name()
        );
        eprintln!("{}", graph);
    }

    // Run optimization passes at the chosen tier.
    crate::mir::passes::run_passes(&mut graph, tier, cfg.dump_mir_passes);

    #[cfg(debug_assertions)]
    {
        if let Err(errors) = verify(&graph) {
            let msgs: Vec<_> = errors.iter().map(|e| e.to_string()).collect();
            return Err(JitError::MirVerification(msgs.join("; ")));
        }
    }

    let isa = create_host_isa()?;
    let clif_func = lower_mir_to_clif(&graph, isa.as_ref())?;
    let compiled = compile_clif_function(clif_func, isa, &[])?;

    let duration_ns = start.elapsed().as_nanos() as u64;
    let tier_num = if tier == crate::mir::passes::PassTier::Optimized {
        2
    } else {
        1
    };
    telemetry::record_compile_time(tier_num == 1, duration_ns);
    telemetry::record_function_compiled(
        function.name().unwrap_or("<anonymous>"),
        tier_num,
        duration_ns,
        compiled.code_size,
    );
    Ok(compiled)
}

/// Extract legacy PropertyInlineCache array from FeedbackVector.
fn extract_property_profile(
    function: &vm::Function,
    feedback: &vm::feedback::FeedbackVector,
) -> Vec<Option<vm::PropertyInlineCache>> {
    let len = function.feedback().len();
    (0..len)
        .map(|i| {
            let id = vm::feedback::FeedbackSlotId(i as u16);
            feedback.property(id).and_then(|p| p.as_monomorphic())
        })
        .collect()
}

/// Compile a profiled VM function into machine code for the Tier 1 subset.
pub fn compile_function_profiled(
    function: &vm::Function,
    property_profile: &[Option<vm::PropertyInlineCache>],
) -> Result<CompiledFunction, JitError> {
    let cfg = crate::config::jit_config();
    let start = Instant::now();

    // v2 (Ignition-style accumulator) baseline probe. Identical purpose
    // to the feedback-aware path — lets unfeedback'd v2 functions hit
    // the x21-pinned stencil directly on first compile.
    if let Some(compiled) = try_compile_v2_template(function) {
        let duration_ns = start.elapsed().as_nanos() as u64;
        telemetry::record_compile_time(true, duration_ns);
        let function_name = function.name().unwrap_or("<anonymous>");
        telemetry::record_function_compiled(function_name, 1, duration_ns, compiled.code_size);
        if cfg.dump_bytecode {
            eprintln!(
                "[JIT] v2 template baseline (profile) for {:?}: {:?}",
                function.name(),
                compiled.origin,
            );
        }
        return Ok(compiled);
    }

    let tier1_strategy = select_tier1_strategy(function, property_profile);
    let function_name = function.name().unwrap_or("<anonymous>");

    if cfg.dump_bytecode {
        eprintln!(
            "[JIT] === Bytecode for {:?} ({} instructions, tier1={:?}) ===",
            function.name(),
            function.bytecode().len(),
            tier1_strategy,
        );
        for (pc, instr) in function.bytecode().instructions().iter().enumerate() {
            eprintln!(
                "  {:04}: {:?} a={} b={} c={}",
                pc,
                instr.opcode(),
                instr.a(),
                instr.b(),
                instr.c()
            );
        }
    }

    if cfg.dump_asm
        && tier1_strategy == Tier1Strategy::TemplateBaseline
        && let Ok(program) = analyze_template_candidate(function, property_profile)
        && let Ok(stencil) = emit_template_stencil(&program)
    {
        eprintln!(
            "[JIT] === Template Baseline Stencil for {:?} ({} bytes) ===",
            function.name(),
            stencil.len(),
        );
        crate::codegen::disasm::dump_disassembly(
            stencil.bytes(),
            0,
            Some("template-baseline-stencil"),
        );
    }

    if tier1_strategy == Tier1Strategy::TemplateBaseline
        && let Ok(program) = analyze_template_candidate(function, property_profile)
        && let Ok(stencil) = emit_template_stencil(&program)
    {
        match compile_code_buffer(&stencil, CompiledCodeOrigin::TemplateBaseline) {
            Ok(compiled) => {
                let duration_ns = start.elapsed().as_nanos() as u64;
                telemetry::record_compile_time(true, duration_ns);
                telemetry::record_function_compiled(
                    function_name,
                    1,
                    duration_ns,
                    compiled.code_size,
                );
                if cfg.dump_bytecode {
                    eprintln!(
                        "[JIT] tier1 backend for {:?}: {:?}",
                        function.name(),
                        compiled.origin,
                    );
                }
                return Ok(compiled);
            }
            Err(err) => {
                if cfg.dump_bytecode {
                    eprintln!(
                        "[JIT] template baseline install failed for {:?}: {err}; falling back to MIR baseline",
                        function.name(),
                    );
                }
            }
        }
    }

    let mut graph = build_mir(
        function,
        (!property_profile.is_empty()).then_some(property_profile),
    )?;

    if cfg.dump_mir {
        eprintln!(
            "[JIT] === MIR (before passes) for {:?} ===",
            function.name()
        );
        eprintln!("{}", graph);
    }

    // Run MIR optimization passes.
    crate::mir::passes::run_passes(
        &mut graph,
        crate::mir::passes::PassTier::Baseline,
        cfg.dump_mir_passes,
    );

    if cfg.dump_mir && cfg.dump_mir_passes {
        eprintln!("[JIT] === MIR (after passes) for {:?} ===", function.name());
        eprintln!("{}", graph);
    }

    #[cfg(debug_assertions)]
    {
        if let Err(errors) = verify(&graph) {
            let msgs: Vec<_> = errors.iter().map(|e| e.to_string()).collect();
            return Err(JitError::MirVerification(msgs.join("; ")));
        }
    }

    let isa = create_host_isa()?;
    let clif_func = lower_mir_to_clif(&graph, isa.as_ref())?;
    let compiled = compile_clif_function(clif_func, isa, &[])?;

    let duration_ns = start.elapsed().as_nanos() as u64;
    telemetry::record_compile_time(true, duration_ns);
    telemetry::record_function_compiled(
        function_name,
        1, // tier 1
        duration_ns,
        compiled.code_size,
    );
    if cfg.dump_bytecode {
        eprintln!(
            "[JIT] tier1 backend for {:?}: {:?}",
            function.name(),
            compiled.origin,
        );
    }
    Ok(compiled)
}

/// Execute a VM function through the Tier 1 JIT path.
pub fn execute_function(
    function: &vm::Function,
    registers: &mut [vm::RegisterValue],
) -> Result<JitExecResult, JitError> {
    execute_function_with_interrupt(function, registers, std::ptr::null())
}

/// Execute a VM function through the Tier 1 JIT path with an explicit interrupt flag.
pub fn execute_function_with_interrupt(
    function: &vm::Function,
    registers: &mut [vm::RegisterValue],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_function(function)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;
    let mut runtime = vm::RuntimeState::new();
    if let Some(receiver_slot) = function.frame_layout().receiver_slot()
        && matches!(
            registers.get(usize::from(receiver_slot)),
            Some(value) if *value == vm::RegisterValue::undefined()
        )
    {
        let global = runtime.intrinsics().global_object();
        registers[usize::from(receiver_slot)] = vm::RegisterValue::from_object_handle(global.0);
    }
    let this_raw = function
        .frame_layout()
        .receiver_slot()
        .and_then(|slot| registers.get(usize::from(slot)))
        .map_or(vm::RegisterValue::undefined().raw_bits(), |v| v.raw_bits());

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw,
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: function as *const vm::Function as *const (),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: std::ptr::null(),
        runtime_ptr: &mut runtime as *mut vm::RuntimeState as *mut (),
        heap_slots_base: runtime.heap().slots_ptr(),
    };

    telemetry::record_jit_entry();
    telemetry::record_function_jit_entry(function.name().unwrap_or("<anonymous>"), 1);
    let result = unsafe { compiled.call(&mut ctx) };
    if result == BAILOUT_SENTINEL {
        Ok(JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
        })
    } else {
        Ok(JitExecResult::Ok(result))
    }
}

/// Execute a profiled VM function with access to shared runtime state.
pub fn execute_function_profiled_with_runtime(
    module: &vm::Module,
    function_index: vm::FunctionIndex,
    registers: &mut [vm::RegisterValue],
    runtime: &mut vm::RuntimeState,
    property_profile: &[Option<vm::PropertyInlineCache>],
    interrupt_flag: *const u8,
) -> Result<JitExecResult, JitError> {
    let function = module
        .function(function_index)
        .ok_or_else(|| JitError::Internal("vm function index is out of bounds".to_string()))?;
    let required_len = usize::from(function.frame_layout().register_count());
    if registers.len() < required_len {
        return Err(JitError::Internal(format!(
            "register slice too small for vm function: need {}, got {}",
            required_len,
            registers.len()
        )));
    }

    let compiled = compile_function_profiled(function, property_profile)?;
    let register_count = u32::try_from(required_len)
        .map_err(|_| JitError::Internal("register count does not fit into u32".to_string()))?;

    // Resolve `this` from the receiver slot if the function declares one,
    // otherwise default to `undefined`. Global scripts use a receiver slot
    // that points to the global object.
    let this_raw = function
        .frame_layout()
        .receiver_slot()
        .and_then(|slot| registers.get(usize::from(slot)))
        .map_or(vm::RegisterValue::undefined().raw_bits(), |v| v.raw_bits());

    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr().cast::<u64>(),
        local_count: register_count,
        register_count,
        constants: std::ptr::null(),
        this_raw,
        interrupt_flag,
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: function as *const vm::Function as *const (),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: vm::RegisterValue::undefined().raw_bits(),
        home_object_raw: vm::RegisterValue::undefined().raw_bits(),
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: module as *const vm::Module as *const (),
        runtime_ptr: runtime as *mut vm::RuntimeState as *mut (),
        heap_slots_base: runtime.heap().slots_ptr(),
    };

    telemetry::record_jit_entry();
    telemetry::record_function_jit_entry(function.name().unwrap_or("<anonymous>"), 1);
    let result = unsafe { compiled.call(&mut ctx) };
    if result == BAILOUT_SENTINEL {
        Ok(JitExecResult::Bailout {
            bytecode_pc: ctx.bailout_pc,
            reason: crate::BailoutReason::from_raw(ctx.bailout_reason)
                .unwrap_or(crate::BailoutReason::Unsupported),
        })
    } else {
        Ok(JitExecResult::Ok(result))
    }
}
