//! # Otter VM Bytecode
//!
//! This crate defines the bytecode format for the Otter JavaScript/TypeScript runtime.
//!
//! ## Design Principles
//!
//! - **Register-based**: Operations work on virtual registers, not a stack
//! - **Compact**: Variable-length operands to minimize bytecode size
//! - **TypeScript-aware**: Preserves type information for optimization
//! - **Serializable**: Can be cached to disk for fast startup

#![warn(clippy::all)]
#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod constant;
pub mod error;
pub mod function;
pub mod instruction;
pub mod module;
pub mod operand;

pub use constant::{Constant, ConstantPool};
pub use error::BytecodeError;
pub use function::{Function, InlineCacheState, InstructionMetadata, TypeFlags, UpvalueCapture};
pub use instruction::{Instruction, Opcode};
pub use module::Module;
pub use operand::{ConstantIndex, FunctionIndex, JumpOffset, LocalIndex, Register};

/// Bytecode format version
pub const BYTECODE_VERSION: u32 = 1;

/// Magic bytes for bytecode files
pub const BYTECODE_MAGIC: [u8; 8] = *b"OTTERBC\0";
