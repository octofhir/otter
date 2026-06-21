//! Optimizing JIT tier (Maglev-analog) for the Otter VM.
//!
//! A second compiled tier above the baseline: it builds a **typed SSA graph**
//! from a hot function's bytecode, speculates unboxed numeric representations
//! from the interpreter's operand-type feedback, lowers to unboxed arm64 with
//! register allocation, and deoptimizes to the interpreter at a guard's exact PC
//! when a type guard fails. The baseline tier remains the fast fallback and the
//! deopt target.
//!
//! The full pipeline runs end to end: [`build_graph`] constructs the typed SSA
//! over the monomorphic numeric subset (unboxed `int32` and `f64` islands),
//! [`liveness`] / [`regalloc`] assign machine homes (GP + FP register classes),
//! [`deopt`] captures per-guard frame states, and [`emit`] lowers to executable
//! arm64. [`compile`] orchestrates these and returns a
//! [`otter_vm::JitFunctionCode`]; the baseline tier is tried as a fallback for
//! anything the optimizing tier declines with [`Unsupported`].
//!
//! # Contents
//! - [`ir`] — the typed SSA graph (`Graph`, `Block`, `Node`, `Repr`).
//! - [`build_graph`] — bytecode → SSA entry point.
//! - [`compile`] — the full pipeline to executable machine code.
//! - [`Unsupported`] — why a function is outside the optimizing subset.
//!
//! # See also
//! - `OPTIMIZING_TIER.md` Part II — the staged plan this implements.
//! - [`crate::baseline`] — the fallback tier and deopt target.

mod builder;
pub mod deopt;
pub mod emit;
pub mod ir;
pub mod liveness;
pub mod regalloc;

use otter_bytecode::Op;
use otter_vm::JitFunctionView;

/// Why a function (or one of its sites) is outside the optimizing subset. A
/// whole-function signal: the VM falls back to the baseline / interpreter, never
/// a partial compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    /// The function has no instructions.
    Empty,
    /// An opcode outside the supported subset.
    Opcode(Op),
    /// An operand was not in the expected shape (wrong kind / out of range).
    OperandShape(&'static str),
    /// A branch resolved to a byte-PC that is not an instruction boundary.
    BranchTarget(i64),
    /// An arithmetic / comparison site whose operand-type feedback is not
    /// int32-only (unobserved, float, string, mixed, …); carries the raw
    /// feedback bits.
    TypeFeedback(u8),
    /// The function has control flow (more than one basic block); the
    /// single-block emitter cannot lower it yet.
    ControlFlow,
    /// A graph shape that is built but not yet lowered to machine code (a
    /// comparison / boolean result, a phi) in the current emitter.
    Unlowered(&'static str),
    /// A value the emitter must read (an operand, return value, phi edge input,
    /// or deopt frame-state value) has no register-allocation home. Aborting the
    /// whole compile keeps every emitted function correct (no wild access); the
    /// VM falls back to the baseline. Widening coverage is a later concern.
    Unallocated,
}

/// Build the typed SSA graph for `view`, or report why it is outside the
/// optimizing subset.
pub fn build_graph(view: &JitFunctionView) -> Result<ir::Graph, Unsupported> {
    builder::build(view, None)
}

/// Build a typed SSA graph rooted at a loop-header OSR entry.
pub fn build_osr_graph(view: &JitFunctionView, osr_pc: u32) -> Result<ir::Graph, Unsupported> {
    builder::build(view, Some(osr_pc))
}

/// Run the whole optimizing-tier pipeline for `view` — graph construction, SSA
/// liveness, linear-scan register allocation, deopt frame-state capture, and
/// machine-code emission — returning a type-erased [`otter_vm::JitFunctionCode`]
/// or the [`Unsupported`] reason the function is outside the tier (the VM then
/// falls back to the baseline).
pub fn compile(
    view: &JitFunctionView,
    osr_pc: Option<u32>,
) -> Result<std::sync::Arc<dyn otter_vm::JitFunctionCode>, Unsupported> {
    let graph = match osr_pc {
        Some(pc) => build_osr_graph(view, pc)?,
        None => build_graph(view)?,
    };
    // Deopt frame states are computed before register allocation: a value a guard
    // restores is an additional use that must extend its live range, so the
    // allocator keeps it in a home the deopt exit can read.
    let bcl = deopt::bytecode_liveness(view);
    let frames = deopt::capture_frame_states(&graph, &bcl);
    let block_deopts = deopt::capture_deopt_terminators(&graph, &bcl);
    let deopt_uses = deopt::deopt_value_uses(&frames);
    let liveness = liveness::analyze(&graph, &deopt_uses, &block_deopts);
    let alloc = regalloc::allocate(&graph, &liveness, emit::GP_REGS, emit::FP_REGS, &deopt_uses);
    // OSR entries reuse the same register→value environment reconstruction as the
    // deopt frame states, captured at each loop header instead of each guard.
    let osr_entries = deopt::capture_osr_entries(&graph, &bcl);
    let code = emit::emit(
        view,
        &graph,
        &liveness,
        &alloc,
        &frames,
        &block_deopts,
        &osr_entries,
    )?;
    Ok(std::sync::Arc::new(code))
}
