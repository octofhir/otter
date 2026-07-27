//! Backend-independent intermediate representations for JIT analysis.
//!
//! # Contents
//! - [`cfg`] — typed bytecode basic blocks and complete control-flow edges.
//! - [`dom`] — dominator tree and dominance-frontier analyses.
//! - [`deopt_lower`] — concrete exact-PC interpreter-frame reconstruction.
//! - [`frame_state`] — abstract exact-PC interpreter-frame reconstruction.
//! - [`inline`] — verified splice decision over monomorphic call sites.
//! - [`licm`] — settled accesses moved out of loops that cannot change them.
//! - [`liveness`] — backward SSA-value liveness over normal control edges.
//! - [`lower`] — bytecode nodes rewritten into the primitive guard vocabulary.
//! - [`regalloc`] — backend-independent linear-scan SSA register allocation.
//! - [`repr`] — feedback-guided SSA representation selection and conversions.
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
pub mod deopt_lower;
pub mod dom;
pub mod frame_state;
pub mod inline;
pub mod licm;
pub mod liveness;
pub mod lower;
pub mod regalloc;
pub mod repr;
pub mod safepoint;
pub mod ssa;
