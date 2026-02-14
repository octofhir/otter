//! # Otter VM JIT
//!
//! Baseline Cranelift-backed JIT infrastructure for hot bytecode functions.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod bailout;
pub mod compiler;
pub mod translator;

pub use bailout::{BAILOUT_SENTINEL, BailoutReason, DEOPT_THRESHOLD, is_bailout};
pub use compiler::{JitCompileArtifact, JitCompiler, JitError};
