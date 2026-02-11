//! Constant pool for bytecode modules

use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

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
    /// Tagged template site data (UTF-16 code units)
    TemplateLiteral {
        /// Unique template site id within a compiled module
        site_id: u32,
        /// Cooked template parts (`undefined` for invalid escape sequences)
        cooked: Vec<Option<Vec<u16>>>,
        /// Raw template parts
        raw: Vec<Vec<u16>>,
    },
    /// Symbol ID (for private fields)
    Symbol(u64),
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

    /// Create a tagged template literal constant
    #[inline]
    pub fn template_literal(
        site_id: u32,
        cooked: Vec<Option<Vec<u16>>>,
        raw: Vec<Vec<u16>>,
    ) -> Self {
        Self::TemplateLiteral {
            site_id,
            cooked,
            raw,
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

    /// Compute a hash for deduplication purposes.
    ///
    /// This implements custom hashing because f64 doesn't implement Hash.
    /// For NaN values, we use a fixed hash.
    fn hash_for_dedup<H: Hasher>(&self, state: &mut H) {
        // Discriminant first
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Number(n) => {
                // Handle NaN specially: all NaN values hash the same
                // Use bit representation for consistent hashing
                n.to_bits().hash(state);
            }
            Self::String(s) => {
                s.hash(state);
            }
            Self::BigInt(s) => {
                s.hash(state);
            }
            Self::RegExp { pattern, flags } => {
                pattern.hash(state);
                flags.hash(state);
            }
            Self::TemplateLiteral {
                site_id,
                cooked,
                raw,
            } => {
                site_id.hash(state);
                cooked.hash(state);
                raw.hash(state);
            }
            Self::Symbol(id) => {
                id.hash(state);
            }
        }
    }
}

/// Constant pool with O(1) hash-based deduplication
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConstantPool {
    constants: Vec<Constant>,
    /// Hash-based deduplication index: hash -> list of indices with that hash
    /// We use a list because different constants can have the same hash (collision).
    #[serde(skip)]
    dedup_index: FxHashMap<u64, Vec<u32>>,
}

impl ConstantPool {
    /// Create a new empty constant pool
    pub fn new() -> Self {
        Self {
            constants: Vec::new(),
            dedup_index: FxHashMap::default(),
        }
    }

    /// Create constant pool with pre-allocated capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            constants: Vec::with_capacity(capacity),
            dedup_index: FxHashMap::with_capacity_and_hasher(capacity, Default::default()),
        }
    }

    /// Compute hash of a constant for deduplication
    #[inline]
    fn hash_constant(constant: &Constant) -> u64 {
        let mut hasher = rustc_hash::FxHasher::default();
        constant.hash_for_dedup(&mut hasher);
        hasher.finish()
    }

    /// Add a constant to the pool, returns its index
    ///
    /// Deduplicates identical constants using O(1) hash-based lookup.
    pub fn add(&mut self, constant: Constant) -> u32 {
        let hash = Self::hash_constant(&constant);

        // Check if we have any constants with this hash
        if let Some(indices) = self.dedup_index.get(&hash) {
            // Check for exact match among hash collisions
            for &idx in indices {
                if self.constants[idx as usize] == constant {
                    return idx;
                }
            }
        }

        // Add new constant
        let idx = self.constants.len() as u32;
        self.constants.push(constant);
        self.dedup_index.entry(hash).or_default().push(idx);
        idx
    }

    /// Rebuild the dedup index after deserialization
    pub fn rebuild_dedup_index(&mut self) {
        self.dedup_index.clear();
        for (idx, constant) in self.constants.iter().enumerate() {
            let hash = Self::hash_constant(constant);
            self.dedup_index.entry(hash).or_default().push(idx as u32);
        }
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
