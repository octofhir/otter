//! Bytecode operands

use serde::{Deserialize, Serialize};

/// Virtual register (0-255)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct Register(pub u8);

impl Register {
    /// Create a new register
    #[inline]
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// Get register index
    #[inline]
    pub const fn index(self) -> u8 {
        self.0
    }
}

impl From<u8> for Register {
    fn from(index: u8) -> Self {
        Self(index)
    }
}

/// Index into constant pool (variable-length encoding)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct ConstantIndex(pub u32);

impl ConstantIndex {
    /// Create a new constant index
    #[inline]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Get index value
    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Index into local variables
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct LocalIndex(pub u16);

impl LocalIndex {
    /// Create a new local index
    #[inline]
    pub const fn new(index: u16) -> Self {
        Self(index)
    }

    /// Get index value
    #[inline]
    pub const fn index(self) -> u16 {
        self.0
    }
}

/// Index into function table
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct FunctionIndex(pub u32);

impl FunctionIndex {
    /// Create a new function index
    #[inline]
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    /// Get index value
    #[inline]
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Jump offset (signed)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct JumpOffset(pub i32);

impl JumpOffset {
    /// Create a new jump offset
    #[inline]
    pub const fn new(offset: i32) -> Self {
        Self(offset)
    }

    /// Get offset value
    #[inline]
    pub const fn offset(self) -> i32 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register() {
        let r = Register::new(5);
        assert_eq!(r.index(), 5);
    }

    #[test]
    fn test_constant_index() {
        let c = ConstantIndex::new(1000);
        assert_eq!(c.index(), 1000);
    }
}
