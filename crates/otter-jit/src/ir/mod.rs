//! Backend-independent intermediate representations for JIT analysis.
//!
//! # Contents
//! - [`cfg`] — typed bytecode basic blocks and complete control-flow edges.
//! - [`dom`] — dominator tree and dominance-frontier analyses.
//! - [`frame_state`] — abstract exact-PC interpreter-frame reconstruction.
//! - [`liveness`] — backward SSA-value liveness over normal control edges.
//! - [`regalloc`] — backend-independent linear-scan SSA register allocation.
//! - [`safepoint`] — precise SSA root sets live across GC safepoints.
//! - [`ssa`] — Cytron SSA construction over bytecode virtual registers.
//!
//! # Invariants
//! - IR construction consumes immutable VM snapshots and has no runtime effect.
//! - Canonical instruction PCs are logical instruction indices, never byte PCs.
//!
//! # See also
//! - [`otter_vm::JitCompileSnapshot`]
//! - [`crate::template`]

pub mod cfg;
pub mod dom;
pub mod frame_state;
pub mod liveness;
pub mod regalloc;
pub mod safepoint;
pub mod ssa;
