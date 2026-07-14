//! Backend-independent intermediate representations for JIT analysis.
//!
//! # Contents
//! - [`cfg`] — typed bytecode basic blocks and complete control-flow edges.
//! - [`dom`] — dominator tree and dominance-frontier analyses.
//! - [`liveness`] — backward SSA-value liveness over normal control edges.
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
pub mod liveness;
pub mod ssa;
