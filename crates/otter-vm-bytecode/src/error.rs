//! Bytecode errors

use thiserror::Error;

/// Errors that can occur during bytecode operations
#[derive(Debug, Error)]
pub enum BytecodeError {
    /// Invalid magic bytes in bytecode file
    #[error("Invalid magic bytes")]
    InvalidMagic,

    /// Unsupported bytecode version
    #[error("Unsupported version: {0}")]
    UnsupportedVersion(u32),

    /// Invalid opcode
    #[error("Invalid opcode: {0}")]
    InvalidOpcode(u8),

    /// Invalid operand
    #[error("Invalid operand at offset {0}")]
    InvalidOperand(usize),

    /// Unexpected end of bytecode
    #[error("Unexpected end of bytecode")]
    UnexpectedEnd,

    /// IO error during serialization
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result type for bytecode operations
pub type Result<T> = std::result::Result<T, BytecodeError>;
