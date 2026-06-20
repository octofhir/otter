//! Optimizing JIT tier (Maglev-analog) for the Otter VM.
//!
//! A second compiled tier above the baseline: it builds a **typed SSA graph**
//! from a hot function's bytecode, speculates unboxed numeric representations
//! from the interpreter's operand-type feedback, and (in later steps) lowers to
//! unboxed arm64 with register allocation and deoptimizes to the interpreter
//! when a type guard fails. The baseline tier remains the fast fallback and the
//! deopt target.
//!
//! This module currently implements the **graph construction** step
//! ([`build_graph`]): bytecode → typed SSA over the int32-numeric monomorphic
//! subset. It emits no machine code yet, so it is not wired into the tier-up
//! ladder — lowering + execution land in the next step. Anything outside the
//! subset returns [`Unsupported`] and the VM keeps using the baseline /
//! interpreter unchanged.
//!
//! # Contents
//! - [`ir`] — the typed SSA graph (`Graph`, `Block`, `Node`, `Repr`).
//! - [`build_graph`] — bytecode → SSA entry point.
//! - [`Unsupported`] — why a function is outside the optimizing subset.
//!
//! # See also
//! - `OPTIMIZING_TIER.md` Part II — the staged plan this implements.
//! - [`crate::baseline`] — the fallback tier and deopt target.

mod builder;
pub mod ir;
pub mod liveness;

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
}

/// Build the typed SSA graph for `view`, or report why it is outside the
/// optimizing subset.
pub fn build_graph(view: &JitFunctionView) -> Result<ir::Graph, Unsupported> {
    builder::build(view)
}
