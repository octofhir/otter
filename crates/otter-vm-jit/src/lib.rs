//! # Otter VM JIT
//!
//! Baseline Cranelift-backed JIT infrastructure for hot bytecode functions.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod bailout;
pub mod compiler;
pub mod translator;
mod type_guards;

pub use bailout::{BAILOUT_SENTINEL, BailoutReason, DEOPT_THRESHOLD, is_bailout};
pub use compiler::{JitCompileArtifact, JitCompiler, JitError};
