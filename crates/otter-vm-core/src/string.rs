//! Interned JavaScript strings
//!
//! Strings are immutable and interned for deduplication.
//! This allows fast equality comparison (pointer comparison).
//!
//! ## GC Integration
//!
//! Strings are managed via `GcRef<JsString>` which wraps a `GcBox<JsString>`.
//! The `GcBox` provides the GC header for marking. Interned strings are kept
//! alive by the global intern table (acting as a GC root).

use crate::gc::GcRef;
use dashmap::DashMap;
use rustc_hash::FxHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

/// Global string intern table
///
/// Stores `GcRef<JsString>` which are Copy (raw pointers).
/// The backing `GcBox` memory is leaked (kept alive forever) for interned strings.
/// This is acceptable since interned strings are typically long-lived.
///
/// Uses a Vec to handle hash collisions by chaining.
static STRING_TABLE: std::sync::LazyLock<DashMap<u64, Vec<GcRef<JsString>>>> =
    std::sync::LazyLock::new(DashMap::new);

/// String interning table for explicit management
///
/// This provides an instance-based alternative to the global STRING_TABLE.
/// Useful for VM instances that want isolated string tables.
pub struct StringTable {
    strings: DashMap<u64, Vec<GcRef<JsString>>>,
}

impl StringTable {
    /// Create a new string table
    pub fn new() -> Self {
        Self {
            strings: DashMap::new(),
        }
    }

    /// Intern a string in this table
    pub fn intern(&self, s: &str) -> GcRef<JsString> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let hash = JsString::compute_hash_units(&units);

        // Check if already interned
        if let Some(bucket) = self.strings.get(&hash) {
            for existing in bucket.iter() {
                if existing.data.as_ref() == units.as_slice() {
                    return *existing;
                }
            }
        }

        // Create new interned string
        let js_str = GcRef::new(JsString {
            data: units.into(),
            utf8: OnceLock::new(),
            hash,
        });

        // Add to the hash bucket
        self.strings
            .entry(hash)
            .or_insert_with(Vec::new)
            .push(js_str);
        js_str
    }

    /// Check if a string is interned in this table
    pub fn is_interned(&self, s: &str) -> bool {
        let units: Vec<u16> = s.encode_utf16().collect();
        let hash = JsString::compute_hash_units(&units);
        if let Some(bucket) = self.strings.get(&hash) {
            for existing in bucket.iter() {
                if existing.data.as_ref() == units.as_slice() {
                    return true;
                }
            }
        }
        false
    }

    /// Intern a UTF-16 string in this table
    pub fn intern_utf16(&self, units: &[u16]) -> GcRef<JsString> {
        let hash = JsString::compute_hash_units(units);

        // Check if already interned
        if let Some(bucket) = self.strings.get(&hash) {
            for existing in bucket.iter() {
                if existing.data.as_ref() == units {
                    return *existing;
                }
            }
        }

        let js_str = GcRef::new(JsString {
            data: Arc::from(units),
            utf8: OnceLock::new(),
            hash,
        });

        // Add to the hash bucket
        self.strings
            .entry(hash)
            .or_insert_with(Vec::new)
            .push(js_str);
        js_str
    }

    /// Get the number of interned strings
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    /// Check if the table is empty
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::new()
    }
}

/// An interned JavaScript string with GC support
///
/// `JsString` is allocated via `GcRef<JsString>` which wraps it in a `GcBox`.
/// The `GcBox` provides the GC header for marking. `JsString` itself only
/// contains the string data and metadata.
#[derive(Clone)]
pub struct JsString {
    /// The actual string data (UTF-16 code units)
    data: Arc<[u16]>,
    /// Cached UTF-8 representation (lossy for lone surrogates)
    utf8: OnceLock<Arc<str>>,
    /// Precomputed hash for fast lookup
    hash: u64,
}

impl JsString {
    /// Create or retrieve an interned string (using global table)
    pub fn intern(s: &str) -> GcRef<Self> {
        let units: Vec<u16> = s.encode_utf16().collect();
        let hash = Self::compute_hash_units(&units);

        // Check if already interned
        if let Some(bucket) = STRING_TABLE.get(&hash) {
            for existing in bucket.iter() {
                if existing.data.as_ref() == units.as_slice() {
                    return *existing;
                }
            }
        }

        // Create new interned string via GcRef
        let js_str = GcRef::new(Self {
            data: units.into(),
            utf8: OnceLock::new(),
            hash,
        });

        // Add to the hash bucket
        STRING_TABLE
            .entry(hash)
            .or_insert_with(Vec::new)
            .push(js_str);
        js_str
    }

    /// Create a string without interning (for temporary strings)
    ///
    /// Returns a `GcRef<JsString>` for the new string.
    pub fn new_gc(s: impl AsRef<str>) -> GcRef<Self> {
        let units: Vec<u16> = s.as_ref().encode_utf16().collect();
        Self::from_utf16_units_gc(units)
    }

    /// Create a string from UTF-16 code units without interning
    pub fn from_utf16_units(units: Vec<u16>) -> Self {
        let hash = Self::compute_hash_units(&units);
        Self {
            data: units.into(),
            utf8: OnceLock::new(),
            hash,
        }
    }

    /// Create a GcRef<JsString> from UTF-16 code units without interning
    pub fn from_utf16_units_gc(units: Vec<u16>) -> GcRef<Self> {
        GcRef::new(Self::from_utf16_units(units))
    }

    /// Create or retrieve an interned string from UTF-16 code units
    pub fn intern_utf16(units: &[u16]) -> GcRef<Self> {
        let hash = Self::compute_hash_units(units);

        // Check if already interned
        if let Some(bucket) = STRING_TABLE.get(&hash) {
            for existing in bucket.iter() {
                if existing.data.as_ref() == units {
                    return *existing;
                }
            }
        }

        let js_str = GcRef::new(Self {
            data: Arc::from(units),
            utf8: OnceLock::new(),
            hash,
        });

        // Add to the hash bucket
        STRING_TABLE
            .entry(hash)
            .or_insert_with(Vec::new)
            .push(js_str);
        js_str
    }

    /// Get the string as a str slice
    #[inline]
    pub fn as_str(&self) -> &str {
        let cached = self.utf8.get_or_init(|| {
            let s = String::from_utf16_lossy(&self.data);
            Arc::<str>::from(s)
        });
        cached.as_ref()
    }

    /// Get the UTF-16 code units
    #[inline]
    pub fn as_utf16(&self) -> &[u16] {
        &self.data
    }

    /// Get the length in UTF-16 code units (for JS compatibility)
    pub fn len_utf16(&self) -> usize {
        self.data.len()
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
    pub fn concat(&self, other: &JsString) -> GcRef<Self> {
        let mut units = Vec::with_capacity(self.data.len() + other.data.len());
        units.extend_from_slice(&self.data);
        units.extend_from_slice(&other.data);
        Self::intern_utf16(&units)
    }

    /// Get character at index (UTF-16 code unit)
    pub fn char_at(&self, index: usize) -> Option<char> {
        self.data
            .get(index)
            .and_then(|unit| char::from_u32(*unit as u32))
    }

    /// Get substring (character-based)
    pub fn substring(&self, start: usize, end: usize) -> GcRef<Self> {
        let s = self.as_str();
        let chars: Vec<char> = s.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let result: String = chars[start..end].iter().collect();
        Self::intern(&result)
    }

    /// Get substring with UTF-16 semantics (for JS String.prototype.substring)
    ///
    /// JavaScript strings use UTF-16 internally, so indices are in UTF-16 code units.
    pub fn substring_utf16(&self, start: usize, end: usize) -> GcRef<Self> {
        let start = start.min(self.data.len());
        let end = end.min(self.data.len()).max(start);
        let slice = &self.data[start..end];
        Self::intern_utf16(slice)
    }

    /// Concatenate using a string table instead of global intern
    pub fn concat_with_table(&self, other: &JsString, table: &StringTable) -> GcRef<Self> {
        let mut units = Vec::with_capacity(self.data.len() + other.data.len());
        units.extend_from_slice(&self.data);
        units.extend_from_slice(&other.data);
        table.intern_utf16(&units)
    }

    fn compute_hash_units(units: &[u16]) -> u64 {
        let mut hasher = FxHasher::default();
        units.hash(&mut hasher);
        hasher.finish()
    }
}

impl std::fmt::Debug for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JsString({:?})", self.as_str())
    }
}

impl std::fmt::Display for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
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
        self.as_str()
    }
}

impl AsRef<[u16]> for JsString {
    fn as_ref(&self) -> &[u16] {
        &self.data
    }
}

// Note: JsString no longer implements GcObject directly.
// The GcBox<JsString> wrapper (via GcRef) provides the GC header.
// JsString::trace is a no-op since strings don't contain GC references.

/// Well-known interned strings (for property names)
///
/// These are lazily initialized and stored as `GcRef<JsString>`.
/// Since `GcRef` is `Copy`, we store them directly without `LazyLock`.
pub mod well_known {
    use super::*;
    use std::sync::LazyLock;

    macro_rules! well_known_string {
        ($name:ident, $value:literal) => {
            /// Well-known string constant
            pub static $name: LazyLock<GcRef<JsString>> =
                LazyLock::new(|| JsString::intern($value));
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

        // Should be the same GcRef (interned) - same pointer
        assert_eq!(s1.as_ptr(), s2.as_ptr());
    }

    #[test]
    fn test_different_strings() {
        let s1 = JsString::intern("hello");
        let s2 = JsString::intern("world");

        assert_ne!(s1.as_ptr(), s2.as_ptr());
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

    #[test]
    fn test_string_table() {
        let table = StringTable::new();

        let s1 = table.intern("hello");
        let s2 = table.intern("hello");
        let s3 = table.intern("world");

        // Same string should return same GcRef (same pointer)
        assert_eq!(s1.as_ptr(), s2.as_ptr());
        // Different string should be different
        assert_ne!(s1.as_ptr(), s3.as_ptr());

        assert!(table.is_interned("hello"));
        assert!(table.is_interned("world"));
        assert!(!table.is_interned("foo"));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn test_substring_utf16() {
        let s = JsString::intern("hello world");
        let sub = s.substring_utf16(0, 5);
        assert_eq!(sub.as_str(), "hello");

        // Test with end > start
        let sub2 = s.substring_utf16(6, 11);
        assert_eq!(sub2.as_str(), "world");
    }

    #[test]
    fn test_substring_utf16_emoji() {
        // Emoji (like 😀) is represented as a surrogate pair in UTF-16
        let s = JsString::intern("a😀b");

        // UTF-16: 'a' (1), surrogate pair for 😀 (2), 'b' (1) = 4 code units
        assert_eq!(s.len_utf16(), 4);

        // Get just 'a'
        let sub = s.substring_utf16(0, 1);
        assert_eq!(sub.as_str(), "a");

        // Get the emoji (needs both surrogate pair code units)
        let sub_emoji = s.substring_utf16(1, 3);
        assert_eq!(sub_emoji.as_str(), "😀");
    }

    #[test]
    fn test_gcref_header() {
        use otter_vm_gc::object::MarkColor;

        let s = JsString::intern("test");

        // GcRef provides header via GcBox
        let header = s.header();

        // Default mark should be white
        assert_eq!(header.mark(), MarkColor::White);
    }

    #[test]
    fn test_concat_with_table() {
        let table = StringTable::new();
        let s1 = table.intern("hello");
        let s2 = table.intern(" world");

        let result = s1.concat_with_table(&s2, &table);
        assert_eq!(result.as_str(), "hello world");

        // Should be interned in the table
        assert!(table.is_interned("hello world"));
    }
}

// ============================================================================
// GC Tracing Implementation
// ============================================================================

impl otter_vm_gc::GcTraceable for JsString {
    // Strings don't contain GC references
    const NEEDS_TRACE: bool = false;

    fn trace(&self, _tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // No references to trace
    }
}

/// Trace all interned strings in the global STRING_TABLE
///
/// This must be called during GC root collection to prevent
/// interned strings from being incorrectly collected.
pub fn trace_global_string_table(tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
    for bucket in STRING_TABLE.iter() {
        for js_str in bucket.value().iter() {
            tracer(js_str.header() as *const _);
        }
    }
}
