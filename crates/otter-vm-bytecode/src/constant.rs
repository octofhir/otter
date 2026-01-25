//! Constant pool for bytecode modules

use serde::{Deserialize, Serialize};

/// A constant value in the constant pool
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Constant {
    /// 64-bit floating point number
    Number(f64),
    /// String value (UTF-16 code units)
    String(Vec<u16>),
    /// BigInt value
    BigInt(Box<str>),
    /// Regular expression
    RegExp {
        /// The regex pattern
        pattern: Box<str>,
        /// The regex flags (e.g., "gi")
        flags: Box<str>,
    },
    /// Template literal parts (UTF-16 code units)
    TemplateLiteral(Vec<Vec<u16>>),
}

impl Constant {
    /// Create a number constant
    #[inline]
    pub fn number(n: f64) -> Self {
        Self::Number(n)
    }

    /// Create a string constant from UTF-16 units
    #[inline]
    pub fn string(units: impl Into<Vec<u16>>) -> Self {
        Self::String(units.into())
    }

    /// Create a string constant from UTF-8 text
    #[inline]
    pub fn string_from_str(s: &str) -> Self {
        Self::String(s.encode_utf16().collect())
    }

    /// Create a BigInt constant
    #[inline]
    pub fn bigint(s: impl Into<Box<str>>) -> Self {
        Self::BigInt(s.into())
    }

    /// Create a RegExp constant
    #[inline]
    pub fn regexp(pattern: impl Into<Box<str>>, flags: impl Into<Box<str>>) -> Self {
        Self::RegExp {
            pattern: pattern.into(),
            flags: flags.into(),
        }
    }

    /// Check if this is a number
    #[inline]
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Number(_))
    }

    /// Check if this is a string
    #[inline]
    pub fn is_string(&self) -> bool {
        matches!(self, Self::String(_))
    }

    /// Get as number if this is a number constant
    #[inline]
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Get as string if this is a string constant
    #[inline]
    pub fn as_string(&self) -> Option<&[u16]> {
        match self {
            Self::String(s) => Some(s.as_slice()),
            _ => None,
        }
    }
}

/// Constant pool with deduplication
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConstantPool {
    constants: Vec<Constant>,
}

impl ConstantPool {
    /// Create a new empty constant pool
    pub fn new() -> Self {
        Self {
            constants: Vec::new(),
        }
    }

    /// Create constant pool with pre-allocated capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            constants: Vec::with_capacity(capacity),
        }
    }

    /// Add a constant to the pool, returns its index
    ///
    /// Deduplicates identical constants to save space.
    pub fn add(&mut self, constant: Constant) -> u32 {
        // Check for existing identical constant
        for (idx, existing) in self.constants.iter().enumerate() {
            if *existing == constant {
                return idx as u32;
            }
        }

        // Add new constant
        let idx = self.constants.len() as u32;
        self.constants.push(constant);
        idx
    }

    /// Add a number constant
    #[inline]
    pub fn add_number(&mut self, n: f64) -> u32 {
        self.add(Constant::number(n))
    }

    /// Add a string constant from UTF-8 text
    #[inline]
    pub fn add_string(&mut self, s: &str) -> u32 {
        self.add(Constant::string_from_str(s))
    }

    /// Add a UTF-16 string constant
    #[inline]
    pub fn add_string_units(&mut self, units: Vec<u16>) -> u32 {
        self.add(Constant::string(units))
    }

    /// Get a constant by index
    #[inline]
    pub fn get(&self, index: u32) -> Option<&Constant> {
        self.constants.get(index as usize)
    }

    /// Number of constants in the pool
    #[inline]
    pub fn len(&self) -> usize {
        self.constants.len()
    }

    /// Check if the pool is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.constants.is_empty()
    }

    /// Iterate over constants
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &Constant> {
        self.constants.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_pool_dedup() {
        let mut pool = ConstantPool::new();

        let idx1 = pool.add_string("hello");
        let idx2 = pool.add_string("world");
        let idx3 = pool.add_string("hello"); // duplicate

        assert_eq!(idx1, 0);
        assert_eq!(idx2, 1);
        assert_eq!(idx3, 0); // same as idx1
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn test_constant_pool_number() {
        let mut pool = ConstantPool::new();

        let idx1 = pool.add_number(42.0);
        let idx2 = pool.add_number(3.15);
        let idx3 = pool.add_number(42.0); // duplicate

        assert_eq!(idx1, 0);
        assert_eq!(idx2, 1);
        assert_eq!(idx3, 0);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn test_constant_get() {
        let mut pool = ConstantPool::new();
        pool.add_string("test");
        pool.add_number(123.0);

        assert_eq!(pool.get(0), Some(&Constant::string_from_str("test")));
        assert_eq!(pool.get(1), Some(&Constant::Number(123.0)));
        assert_eq!(pool.get(2), None);
    }
}
