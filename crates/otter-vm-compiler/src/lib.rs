//! # Otter VM Compiler
//!
//! Compiles JavaScript/TypeScript source code to bytecode using oxc parser.
//!
//! ## Pipeline
//!
//! 1. Parse source with oxc
//! 2. Walk AST and generate bytecode
//! 3. Optimize bytecode (optional)
//! 4. Serialize to module format

#![warn(clippy::all)]
#![warn(missing_docs)]

pub mod codegen;
pub mod compiler;
pub mod error;
pub mod literal_validator;
pub mod peephole;
pub mod scope;

pub use compiler::Compiler;
pub use error::{CompileError, CompileResult};
pub use literal_validator::{EcmaVersion, LiteralValidator, SourceLocation, ValidationContext};
pub use peephole::PeepholeOptimizer;
