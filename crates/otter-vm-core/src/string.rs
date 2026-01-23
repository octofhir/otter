//! Interned JavaScript strings
//!
//! Strings are immutable and interned for deduplication.
//! This allows fast equality comparison (pointer comparison).

use dashmap::DashMap;
use otter_vm_gc::object::tags;
use otter_vm_gc::{GcHeader, GcObject};
use rustc_hash::FxHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Global string intern table
static STRING_TABLE: std::sync::LazyLock<DashMap<u64, Arc<JsString>>> =
    std::sync::LazyLock::new(DashMap::new);

/// String interning table for explicit management
///
/// This provides an instance-based alternative to the global STRING_TABLE.
/// Useful for VM instances that want isolated string tables.
pub struct StringTable {
    strings: DashMap<u64, Arc<JsString>>,
}

impl StringTable {
    /// Create a new string table
    pub fn new() -> Self {
        Self {
            strings: DashMap::new(),
        }
    }

    /// Intern a string in this table
    pub fn intern(&self, s: &str) -> Arc<JsString> {
        let hash = JsString::compute_hash(s);

        // Check if already interned
        if let Some(existing) = self.strings.get(&hash)
            && existing.data.as_ref() == s
        {
            return existing.clone();
        }

        // Create new interned string
        let js_str = Arc::new(JsString {
            header: GcHeader::new(tags::STRING),
            data: Arc::from(s),
            hash,
        });

        self.strings.insert(hash, js_str.clone());
        js_str
    }

    /// Check if a string is interned in this table
    pub fn is_interned(&self, s: &str) -> bool {
        let hash = JsString::compute_hash(s);
        self.strings.contains_key(&hash)
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
#[repr(C)]
#[derive(Clone)]
pub struct JsString {
    /// GC header for garbage collection
    header: GcHeader,
    /// The actual string data
    data: Arc<str>,
    /// Precomputed hash for fast lookup
    hash: u64,
}

impl JsString {
    /// Create or retrieve an interned string (using global table)
    pub fn intern(s: &str) -> Arc<Self> {
        let hash = Self::compute_hash(s);

        // Check if already interned
        if let Some(existing) = STRING_TABLE.get(&hash)
            && existing.data.as_ref() == s
        {
            return existing.clone();
        }

        // Create new interned string
        let js_str = Arc::new(Self {
            header: GcHeader::new(tags::STRING),
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
        Self {
            header: GcHeader::new(tags::STRING),
            data,
            hash,
        }
    }

    /// Get the GC header
    pub fn gc_header(&self) -> &GcHeader {
        &self.header
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

    /// Get substring (character-based)
    pub fn substring(&self, start: usize, end: usize) -> Arc<Self> {
        let chars: Vec<char> = self.data.chars().collect();
        let start = start.min(chars.len());
        let end = end.min(chars.len()).max(start);
        let s: String = chars[start..end].iter().collect();
        Self::intern(&s)
    }

    /// Get substring with UTF-16 semantics (for JS String.prototype.substring)
    ///
    /// JavaScript strings use UTF-16 internally, so indices are in UTF-16 code units.
    pub fn substring_utf16(&self, start: usize, end: usize) -> Arc<Self> {
        let utf16: Vec<u16> = self.data.encode_utf16().collect();
        let start = start.min(utf16.len());
        let end = end.min(utf16.len()).max(start);
        let slice = &utf16[start..end];
        let result = String::from_utf16_lossy(slice);
        Self::intern(&result)
    }

    /// Concatenate using a string table instead of global intern
    pub fn concat_with_table(&self, other: &JsString, table: &StringTable) -> Arc<Self> {
        let mut result = String::with_capacity(self.len() + other.len());
        result.push_str(&self.data);
        result.push_str(&other.data);
        table.intern(&result)
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

// GC integration
impl GcObject for JsString {
    fn header(&self) -> &GcHeader {
        &self.header
    }

    fn trace(&self, _tracer: &mut dyn FnMut(*const GcHeader)) {
        // Strings don't contain references to other GC objects
        // The Arc<str> data is managed by Rust's reference counting
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

    #[test]
    fn test_string_table() {
        let table = StringTable::new();

        let s1 = table.intern("hello");
        let s2 = table.intern("hello");
        let s3 = table.intern("world");

        // Same string should return same Arc
        assert!(Arc::ptr_eq(&s1, &s2));
        // Different string should be different
        assert!(!Arc::ptr_eq(&s1, &s3));

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
    fn test_gc_header() {
        use otter_vm_gc::object::MarkColor;

        let s = JsString::intern("test");
        let header = s.gc_header();

        // Default mark should be white
        assert_eq!(header.mark(), MarkColor::White);
        assert_eq!(header.tag(), tags::STRING);
    }

    #[test]
    fn test_gc_object_trait() {
        let s = JsString::intern("test");

        // Test header() method from GcObject trait
        let header = GcObject::header(s.as_ref());
        assert_eq!(header.tag(), tags::STRING);

        // Test trace() - should not panic
        GcObject::trace(s.as_ref(), &mut |_ptr| {
            // Strings don't have references, so this should never be called
            panic!("Strings should not trace any references");
        });
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
