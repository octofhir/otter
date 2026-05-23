//! # Otter VM JIT
//!
//! Baseline Cranelift-backed JIT infrastructure for hot bytecode functions.
//! Includes NaN-boxing-aware type guards for speculative i32 fast paths.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod bailout;
pub mod compiler;
pub mod runtime_helpers;
pub mod translator;
pub mod type_guards;

pub use bailout::{is_bailout, BAILOUT_SENTINEL, DEOPT_THRESHOLD};
pub use compiler::{JitCompileArtifact, JitCompiler, JitError};
