//! Interned JavaScript strings
//!
//! Strings are immutable and interned for deduplication.
//! This allows fast equality comparison (pointer comparison).

use dashmap::DashMap;
use rustc_hash::FxHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Global string intern table
static STRING_TABLE: std::sync::LazyLock<DashMap<u64, Arc<JsString>>> =
    std::sync::LazyLock::new(DashMap::new);

/// An interned JavaScript string
#[derive(Clone)]
pub struct JsString {
    /// The actual string data
    data: Arc<str>,
    /// Precomputed hash for fast lookup
    hash: u64,
}

impl JsString {
    /// Create or retrieve an interned string
    pub fn intern(s: &str) -> Arc<Self> {
        let hash = Self::compute_hash(s);

        // Check if already interned
        if let Some(existing) = STRING_TABLE.get(&hash) {
            if existing.data.as_ref() == s {
                return existing.clone();
            }
        }

        // Create new interned string
        let js_str = Arc::new(Self {
            data: Arc::from(s),
            hash,
        });

        STRING_TABLE.insert(hash, js_str.clone());
        js_str
    }

    /// Create a string without interning (for temporary strings)
    pub fn new(s: impl Into<Arc<str>>) -> Self {
        let data: Arc<str> = s.into();
        let hash = Self::compute_hash(&data);
        Self { data, hash }
    }

    /// Get the string as a str slice
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.data
    }

    /// Get the length in UTF-16 code units (for JS compatibility)
    pub fn len_utf16(&self) -> usize {
        self.data.encode_utf16().count()
    }

    /// Get the length in bytes
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if string is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Get precomputed hash value
    #[inline]
    pub fn hash_value(&self) -> u64 {
        self.hash
    }

    /// Concatenate two strings
    pub fn concat(&self, other: &JsString) -> Arc<Self> {
        let mut result = String::with_capacity(self.len() + other.len());
        result.push_str(&self.data);
        result.push_str(&other.data);
        Self::intern(&result)
    }

    /// Get character at index (UTF-16 code unit)
    pub fn char_at(&self, index: usize) -> Option<char> {
        self.data.encode_utf16().nth(index).and_then(|c| {
            // Handle surrogate pairs
            char::from_u32(c as u32)
        })
    }

    /// Get substring
    pub fn substring(&self, start: usize, end: usize) -> Arc<Self> {
        let chars: Vec<char> = self.data.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let s: String = chars[start..end].iter().collect();
        Self::intern(&s)
    }

    fn compute_hash(s: &str) -> u64 {
        let mut hasher = FxHasher::default();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

impl std::fmt::Debug for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsString({:?})", self.data)
    }
}

impl std::fmt::Display for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.data)
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        // Fast path: same hash means likely same string
        if self.hash != other.hash {
            return false;
        }
        // Verify with actual comparison
        self.data == other.data
    }
}

impl Eq for JsString {}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.hash.hash(state);
    }
}

impl AsRef<str> for JsString {
    fn as_ref(&self) -> &str {
        &self.data
    }
}

/// Well-known interned strings (for property names)
pub mod well_known {
    use super::*;
    use std::sync::LazyLock;

    macro_rules! well_known_string {
        ($name:ident, $value:literal) => {
            /// Well-known string constant
            pub static $name: LazyLock<Arc<JsString>> = LazyLock::new(|| JsString::intern($value));
        };
    }

    well_known_string!(LENGTH, "length");
    well_known_string!(PROTOTYPE, "prototype");
    well_known_string!(CONSTRUCTOR, "constructor");
    well_known_string!(NAME, "name");
    well_known_string!(VALUE, "value");
    well_known_string!(WRITABLE, "writable");
    well_known_string!(ENUMERABLE, "enumerable");
    well_known_string!(CONFIGURABLE, "configurable");
    well_known_string!(GET, "get");
    well_known_string!(SET, "set");
    well_known_string!(UNDEFINED, "undefined");
    well_known_string!(NULL, "null");
    well_known_string!(TRUE, "true");
    well_known_string!(FALSE, "false");
    well_known_string!(TO_STRING, "toString");
    well_known_string!(VALUE_OF, "valueOf");
    well_known_string!(CALL, "call");
    well_known_string!(APPLY, "apply");
    well_known_string!(BIND, "bind");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interning() {
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern("hello");

        // Should be the same Arc (interned)
        assert!(Arc::ptr_eq(&s1, &s2));
    }

    #[test]
    fn test_different_strings() {
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern("world");

        assert!(!Arc::ptr_eq(&s1, &s2));
        assert_ne!(s1.hash_value(), s2.hash_value());
    }

    #[test]
    fn test_concat() {
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern(" world");
        let result = s1.concat(&s2);

        assert_eq!(result.as_str(), "hello world");
    }

    #[test]
    fn test_substring() {
        let s = JsString::intern("hello world");
        let sub = s.substring(0, 5);

        assert_eq!(sub.as_str(), "hello");
    }
}
