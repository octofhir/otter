//! # Otter VM JIT
//!
//! Baseline Cranelift-backed JIT infrastructure for hot bytecode functions.

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod compiler;
pub mod translator;

pub use compiler::{JitCompileArtifact, JitCompiler, JitError};
